//! The link of a program with nothing of rustc's (docs/no-rustc.md, step G): blaze's objects
//! and the native libraries the program's crates ask for with `#[link]`, by the system's `cc`,
//! with the arguments rustc gives it for the target. An ELF target links with the toolchain's
//! lld where it has one, as rustc does on x86_64 Linux: GNU ld takes ten times as long to
//! leave out what is not reached.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use hir::Crate;
use hir_def::attrs::{AttrFlags, NativeLib, NativeLibKind};
use ide_db::RootDatabase;
use ide_db::base_db::{self, CrateOrigin, LangCrateOrigin};
use rustc_hash::FxHashSet;

pub(crate) mod split;

/// Links `objects` into the executable `exe`, with the native libraries of the crates `program`
/// links. A shared library instead when `exports` names what it exports: the rest of it is
/// its own, and the link leaves out what that does not reach, as rustc's of a `cdylib` does.
/// `user` is what the flags pass the linker (`-C link-arg`). How long `cc` took.
pub(crate) fn executable(
    db: &RootDatabase,
    program: &[Crate],
    objects: &[&Path],
    sysroot: Option<&Path>,
    exe: &Path,
    exports: Option<&[String]>,
    user: &[String],
    debug: crate::plan::Level,
) -> anyhow::Result<Duration> {
    let mut line = Line::of(db, program, sysroot, exports.is_some(), user, debug)?;
    let Some(exports) = exports else { return line.link(objects, exe) };
    if line.wasm {
        line.after.extend(exports.iter().map(|it| format!("--export={it}")));
        return line.link(objects, exe);
    }
    if line.windows {
        // A DLL names what it exports on the command line; `GetProcAddress` finds the list of
        // a proc-macro crate by it.
        line.before.push("/DLL".to_owned());
        line.before.extend(exports.iter().map(|it| format!("/EXPORT:{it}")));
        return line.link(objects, exe);
    }
    let mut listed = exe.as_os_str().to_owned();
    listed.push(".exports");
    let listed = PathBuf::from(listed);
    let (text, arg) = if line.apple {
        let text: String = exports.iter().map(|it| format!("_{it}\n")).collect();
        (text, format!("-Wl,-exported_symbols_list,{}", listed.display()))
    } else {
        let text: String = exports.iter().map(|it| format!("  {it};\n")).collect();
        (
            format!("{{\n global:\n{text} local: *;\n}};\n"),
            format!("-Wl,--version-script={}", listed.display()),
        )
    };
    std::fs::write(&listed, text).with_context(|| format!("writing {}", listed.display()))?;
    line.after.push(arg);
    let took = line.link(objects, exe);
    _ = std::fs::remove_file(&listed);
    took
}

/// The command that links `objects` into the executable `exe` as [`executable`] does, for a
/// shell: the daemon's line, which its edits are spliced after.
pub(crate) fn command(
    db: &RootDatabase,
    program: &[Crate],
    objects: &[&Path],
    sysroot: Option<&Path>,
    exe: &Path,
    user: &[String],
    debug: crate::plan::Level,
) -> anyhow::Result<String> {
    let line = Line::of(db, program, sysroot, false, user, debug)?;
    if line.windows {
        return line.msvc_command(objects, exe);
    }
    let mut args = vec![line.program.display().to_string()];
    args.extend(line.before.iter().cloned());
    for &object in objects {
        let path = object.display().to_string();
        // An object the build has yet to write is none.
        if object.exists() && split::is_archive(&leading_bytes(object)?) {
            let [whole, no_whole] = line.whole_archive();
            args.extend([whole.to_owned(), path, no_whole.to_owned()]);
        } else {
            args.push(path);
        }
    }
    args.extend(line.after.iter().cloned());
    args.extend(["-o".to_owned(), exe.display().to_string()]);
    if line.apple {
        // Last, as on rustc's line: the room the splice wants, which a link made short of it
        // tells the file, and the link again reads. It is no less than the flags ask for.
        let asked = user.iter().filter_map(|it| crate::cargo::plan::headerpad_in(it)).next_back();
        args.push(crate::cargo::plan::headerpad_read(exe, asked)?);
    }
    shlex::try_join(args.iter().map(String::as_str)).context("a link argument has a NUL in it")
}

/// The arguments `cc` links the program of some crates with, around its objects: of a wasm
/// program, the toolchain's `rust-lld` as `wasm-ld`, which rustc runs itself.
pub(crate) struct Line {
    program: PathBuf,
    before: Vec<String>,
    after: Vec<String>,
    apple: bool,
    wasm: bool,
    /// Linked by `link.exe`: the arguments are its own.
    windows: bool,
    raw_dylibs: Vec<(String, Vec<(String, bool)>)>,
}

impl Line {
    /// The line of `program`, with `user`, what the flags pass the linker (`-C link-arg`),
    /// after the native libraries as rustc has them.
    pub(crate) fn of(
        db: &RootDatabase,
        program: &[Crate],
        sysroot: Option<&Path>,
        shared: bool,
        user: &[String],
        debug: crate::plan::Level,
    ) -> anyhow::Result<Line> {
        let Some(&first) = program.first() else { bail!("nothing to link") };
        let target = Target { shared, ..Target::of(db, first.into()) };
        let mut before = Vec::new();
        let mut linker = PathBuf::from("cc");
        if target.wasm {
            let sysroot = sysroot.context(
                "a wasm program is linked by the toolchain's rust-lld, and there is no toolchain",
            )?;
            linker = rust_lld(sysroot).with_context(|| {
                format!("a wasm program is linked by rust-lld, which {} has not", sysroot.display())
            })?;
            before.extend(["-flavor", "wasm"].map(str::to_owned));
            before.extend(target.wasm_start(sysroot)?);
        } else if !target.apple
            && !target.windows
            && let Some(gcc_ld) = sysroot.and_then(lld_dir)
        {
            before.extend([format!("-B{}", gcc_ld.display()), "-fuse-ld=lld".to_owned()]);
        }
        before.extend(target.leading());
        let native = native(db, program, &target);
        let mut after = native.search;
        if target.wasm
            && let Some(dir) = sysroot.and_then(|it| target.self_contained(it))
        {
            after.push(format!("-L{}", dir.display()));
        }
        for lib in &native.libs {
            after.extend(target.lib(lib));
        }
        after.extend(native.args);
        after.extend(user.iter().cloned());
        after.extend(target.trailing());
        // As cargo's profile has it of a build with no debuginfo (`strip = "debuginfo"`), which
        // rustc tells `wasm-ld`: ELF objects keep the lines of the standard library's unit.
        if target.wasm && debug == crate::plan::Level::Nothing {
            after.push("--strip-debug".to_owned());
        }
        Ok(Line {
            program: linker,
            before,
            after,
            apple: target.apple,
            wasm: target.wasm,
            windows: target.windows,
            raw_dylibs: native.raw_dylibs,
        })
    }

    /// How the linker is told to take every member of an archive, and to stop.
    fn whole_archive(&self) -> [&'static str; 2] {
        match self.wasm {
            true => ["--whole-archive", "--no-whole-archive"],
            false => ["-Wl,--whole-archive", "-Wl,--no-whole-archive"],
        }
    }

    /// Links `objects` into the executable `exe`. How long the linker took.
    pub(crate) fn link(&self, objects: &[&Path], exe: &Path) -> anyhow::Result<Duration> {
        if self.windows {
            return self.msvc_link(objects, exe);
        }
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.before);
        for &object in objects {
            if split::is_archive(&leading_bytes(object)?) {
                let [whole, no_whole] = self.whole_archive();
                cmd.arg(whole).arg(object).arg(no_whole);
            } else {
                cmd.arg(object);
            }
        }
        cmd.args(&self.after).arg("-o").arg(exe);
        let start = Instant::now();
        let ran = blaze_cargo::trace::link_output(&mut cmd, "link executable")
            .with_context(|| format!("running {cmd:?}"))?;
        if !ran.status.success() {
            bail!(
                "the link of {} failed ({}):\n{:?}\n{}",
                exe.display(),
                ran.status,
                cmd,
                String::from_utf8_lossy(&ran.stderr)
            );
        }
        Ok(start.elapsed())
    }
}

impl Line {
    /// The arguments of `link.exe`, in a response file next to `exe`: the line's, the objects
    /// (an archive taken whole, as `--whole-archive` does for `cc`), the directories of the
    /// system's libraries, and the output.
    fn msvc_arguments(&self, objects: &[&Path], exe: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
        let (linker, libs) = msvc_tool()?;
        let mut args: Vec<String> = self.before.clone();
        for &object in objects {
            let path = object.display().to_string();
            if object.exists() && split::is_archive(&leading_bytes(object)?) {
                args.push(format!("/WHOLEARCHIVE:{path}"));
            } else {
                args.push(path);
            }
        }
        args.extend(libs.iter().map(|it| format!("/LIBPATH:{}", it.display())));
        if let Some(dir) = self.import_libraries(&libs, exe)? {
            args.push(format!("/LIBPATH:{}", dir.display()));
        }
        args.extend(self.after.iter().cloned());
        args.push(format!("/OUT:{}", exe.display()));
        let mut response = exe.as_os_str().to_owned();
        response.push(".link.rsp");
        let response = PathBuf::from(response);
        let text: Vec<String> = args.iter().map(|it| format!("\"{it}\"")).collect();
        std::fs::write(&response, text.join("\n"))
            .with_context(|| format!("writing {}", response.display()))?;
        Ok((linker, response))
    }

    /// The import libraries of the DLLs the program imports with `raw-dylib` and the system's
    /// libraries lack, as rustc writes them: made from the names imported, in a directory
    /// beside `exe`. `None` when there is none to make.
    fn import_libraries(&self, system: &[PathBuf], exe: &Path) -> anyhow::Result<Option<PathBuf>> {
        let missing: Vec<_> = self
            .raw_dylibs
            .iter()
            .filter(|(dll, _)| !system.iter().any(|dir| dir.join(format!("{dll}.lib")).is_file()))
            .collect();
        if missing.is_empty() {
            return Ok(None);
        }
        let mut dir = exe.as_os_str().to_owned();
        dir.push(".implib");
        let dir = PathBuf::from(dir);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let Some(tool) = find_msvc_tool("lib.exe") else { bail!("no lib.exe beside link.exe") };
        for (dll, names) in missing {
            let mut names = names.clone();
            names.sort();
            names.dedup();
            let mut def = format!("LIBRARY {dll}.dll\nEXPORTS\n");
            for (name, data) in &names {
                def.push_str(&format!("  {name}{}\n", if *data { " DATA" } else { "" }));
            }
            let file = dir.join(format!("{dll}.def"));
            std::fs::write(&file, def).with_context(|| format!("writing {}", file.display()))?;
            let ran = Command::new(&tool)
                .arg("/NOLOGO")
                .arg(format!("/DEF:{}", file.display()))
                .arg(format!("/OUT:{}", dir.join(format!("{dll}.lib")).display()))
                .arg("/MACHINE:X64")
                .output()
                .with_context(|| format!("running {}", tool.display()))?;
            if !ran.status.success() {
                bail!(
                    "the import library of {dll}.dll failed: {}",
                    String::from_utf8_lossy(&ran.stdout)
                );
            }
        }
        Ok(Some(dir))
    }

    fn msvc_command(&self, objects: &[&Path], exe: &Path) -> anyhow::Result<String> {
        let (linker, response) = self.msvc_arguments(objects, exe)?;
        Ok(format!("\"{}\" \"@{}\"", linker.display(), response.display()))
    }

    fn msvc_link(&self, objects: &[&Path], exe: &Path) -> anyhow::Result<Duration> {
        let (linker, response) = self.msvc_arguments(objects, exe)?;
        let mut cmd = Command::new(&linker);
        cmd.arg(format!("@{}", response.display()));
        let start = Instant::now();
        let ran = blaze_cargo::trace::link_output(&mut cmd, "link executable")
            .with_context(|| format!("running {cmd:?}"))?;
        if !ran.status.success() {
            bail!(
                "the link of {} failed ({}):\n{:?}\n{}{}",
                exe.display(),
                ran.status,
                cmd,
                String::from_utf8_lossy(&ran.stdout),
                String::from_utf8_lossy(&ran.stderr)
            );
        }
        Ok(start.elapsed())
    }
}

/// `link.exe` and the directories it finds the C runtime and the system's libraries in.
#[cfg(windows)]
fn msvc_tool() -> anyhow::Result<(PathBuf, Vec<PathBuf>)> {
    let Some(tool) = find_msvc_tools::find_tool(std::env::consts::ARCH, "link.exe") else {
        bail!("no link.exe: install the MSVC build tools");
    };
    let libs = tool
        .env()
        .into_iter()
        .find(|(name, _)| name == "LIB")
        .map(|(_, dirs)| {
            std::env::split_paths(dirs).filter(|it| !it.as_os_str().is_empty()).collect()
        })
        .unwrap_or_default();
    Ok((tool.path().to_path_buf(), libs))
}

/// A program of the MSVC tools next to `link.exe`.
#[cfg(windows)]
fn find_msvc_tool(name: &str) -> Option<PathBuf> {
    find_msvc_tools::find_tool(std::env::consts::ARCH, name).map(|it| it.path().to_path_buf())
}

#[cfg(not(windows))]
fn find_msvc_tool(_: &str) -> Option<PathBuf> {
    None
}

#[cfg(not(windows))]
fn msvc_tool() -> anyhow::Result<(PathBuf, Vec<PathBuf>)> {
    bail!("link.exe links a Windows program from Windows")
}

/// The first bytes of the file at `path`, which tell an archive from an object.
fn leading_bytes(path: &Path) -> anyhow::Result<[u8; 8]> {
    use std::io::Read;
    let mut bytes = [0; 8];
    let mut file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    match file.read_exact(&mut bytes) {
        Ok(()) => Ok(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok([0; 8]),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// What the stand-ins left in the executable say: the functions blaze could not compile that
/// the program can reach, as the link leaves out every one it can't. They are the lines of the
/// section that holds them alone ([`hir_ty_codegen::stand_in_section`]), so what else the
/// program has in its data (blaze has the words of the message) is never taken for one.
pub(crate) fn stand_ins(exe: &Path) -> anyhow::Result<Vec<String>> {
    use object::{Object, ObjectSection};
    let file = std::fs::File::open(exe).with_context(|| format!("opening {}", exe.display()))?;
    // Mapped, not read: a debug executable is over 100 MB and only the headers and one small
    // section are looked at, by every build that is over, a no-op's too.
    // SAFETY: nothing writes the file meanwhile: who asks linked it (a worker, a doc test), or
    // is a client that holds it unwritten ([`crate::splice::unwritten`]).
    let bytes = unsafe { memmap2::Mmap::map(&file) }
        .with_context(|| format!("reading {}", exe.display()))?;
    if wasmparser::Parser::is_core_wasm(&bytes) {
        let (_, name) = hir_ty_codegen::stand_in_section(object::BinaryFormat::Wasm);
        let segments = wasm_segments_named(&bytes, name)
            .with_context(|| format!("reading the data of {}", exe.display()))?;
        return messages(segments, name, exe);
    }
    let file =
        object::File::parse(&*bytes).with_context(|| format!("reading {}", exe.display()))?;
    let (_, name) = hir_ty_codegen::stand_in_section(file.format());
    let mut sections = Vec::new();
    for section in file.sections().filter(|it| it.name() == Ok(name)) {
        sections
            .push(section.data().with_context(|| format!("reading {name} of {}", exe.display()))?);
    }
    messages(sections, name, exe)
}

/// The data segments of the wasm module `bytes` that the name section calls `name`. `wasm-ld`
/// merges the segments of the objects by name, and names what it merged in the module: the
/// stand-ins' messages are one segment of what nothing else is in.
fn wasm_segments_named<'a>(bytes: &'a [u8], name: &str) -> anyhow::Result<Vec<&'a [u8]>> {
    use wasmparser::{KnownCustom, Name, Payload};
    let mut segments = Vec::new();
    let (mut named, mut names_data) = (Vec::new(), false);
    for payload in wasmparser::Parser::new(0).parse_all(bytes) {
        match payload? {
            Payload::DataSection(reader) => {
                for data in reader {
                    segments.push(data?.data);
                }
            }
            Payload::CustomSection(custom) => {
                let KnownCustom::Name(names) = custom.as_known() else { continue };
                for names in names {
                    let Name::Data(names) = names? else { continue };
                    names_data = true;
                    for naming in names {
                        let naming = naming?;
                        if naming.name == name {
                            named.push(naming.index as usize);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    // Stripped of its names, a module could have stand-ins no one could tell from its data.
    if !segments.is_empty() && !names_data {
        bail!("the module names none of its data segments (was it linked with --strip-all?)");
    }
    named.into_iter().map(|it| segments.get(it).copied().context("a name of no segment")).collect()
}

/// The messages of the stand-ins in the `sections` of `exe` that [`stand_ins`] reads.
fn messages(sections: Vec<&[u8]>, name: &str, exe: &Path) -> anyhow::Result<Vec<String>> {
    let mut said = Vec::new();
    for data in sections {
        for line in data.split(|&it| it == b'\n') {
            // What a linker pads the section with between messages is not a message.
            let line = String::from_utf8_lossy(line);
            let line = line.trim_matches(|it: char| it == '\0' || it.is_ascii_whitespace());
            if line.is_empty() {
                continue;
            }
            if !line.starts_with(hir_ty_codegen::STAND_IN_SAYS) {
                bail!("{name} of {} has what is not a stand-in's message: {line:?}", exe.display());
            }
            said.push(line.to_owned());
        }
    }
    Ok(said)
}

/// The toolchain's directory with lld as `ld.lld`, for `cc -B`: the `gcc-ld` of the host's
/// `lib/rustlib/<host>/bin`.
fn lld_dir(sysroot: &Path) -> Option<PathBuf> {
    std::fs::read_dir(sysroot.join("lib/rustlib"))
        .ok()?
        .filter_map(|it| Some(it.ok()?.path().join("bin/gcc-ld")))
        .find(|it| it.join("ld.lld").is_file())
}

/// The toolchain's `rust-lld`, in the host's `lib/rustlib/<host>/bin`.
fn rust_lld(sysroot: &Path) -> Option<PathBuf> {
    let lld = format!("rust-lld{}", std::env::consts::EXE_SUFFIX);
    std::fs::read_dir(sysroot.join("lib/rustlib"))
        .ok()?
        .filter_map(|it| Some(it.ok()?.path().join("bin").join(&lld)))
        .find(|it| it.is_file())
}

/// What the link line has of the crates `program` links beyond their objects.
#[derive(Default)]
struct Native {
    /// Where the libraries are looked for: `-L` and `-F`.
    search: Vec<String>,
    libs: Vec<NativeLib>,
    /// The DLLs the program imports with `kind = "raw-dylib"` and the names it imports from
    /// each: rustc writes their import libraries, and so must the link on Windows.
    raw_dylibs: Vec<(String, Vec<(String, bool)>)>,
    /// What the build scripts of the program's packages give the linker as it is.
    args: Vec<String>,
}

/// The native libraries of the crates `program` links, as rustc orders them: a crate's before
/// those of the crates it depends on, each crate's in the order written and then those its
/// build script asked for, and each library last where it is asked for more than once.
fn native(db: &RootDatabase, program: &[Crate], target: &Target) -> Native {
    let mut seen = FxHashSet::default();
    let mut order = Vec::new();
    let unlinked = crate::unlinked_panic_runtime(db, program);
    let programs: FxHashSet<base_db::Crate> = program.iter().map(|&it| it.into()).collect();
    for &krate in program {
        depth_first(db, krate.into(), unlinked, &programs, &mut seen, &mut order);
    }
    let mut native = Native::default();
    for &krate in order.iter().rev() {
        let attributes = hir_def::nameres::crate_def_map(db, krate)
            .modules(db)
            .flat_map(|(_, module)| module.scope(db).extern_blocks())
            .flat_map(|block| AttrFlags::native_libs(db, block).iter().cloned());
        native.libs.extend(attributes.collect::<Vec<_>>());
        // A `cdylib` takes `rustc-cdylib-link-arg`; a proc macro is linked shared too, but
        // is not one.
        let cdylib = target.shared && !krate.data(db).target_kind.is_proc_macro();
        let asked = asked_by_script(db, krate, target, cdylib);
        if target.windows {
            native.raw_dylibs.extend(raw_dylibs(db, krate));
        }
        native.libs.extend(asked.libs);
        native.search.extend(asked.search);
        // As cargo has it: a script's arguments are for its own package's targets.
        if programs.contains(&krate) {
            native.args.extend(asked.args);
        }
    }
    let mut later = FxHashSet::default();
    native.libs.reverse();
    native.libs.retain(|lib| later.insert((lib.name.clone(), lib.kind)));
    native.libs.reverse();
    let mut searched = FxHashSet::default();
    native.search.retain(|it| searched.insert(it.clone()));
    native
}

/// The DLLs the extern blocks of `krate` import with `kind = "raw-dylib"`, each with the names
/// imported from it and whether the name is a static (data).
fn raw_dylibs(db: &RootDatabase, krate: base_db::Crate) -> Vec<(String, Vec<(String, bool)>)> {
    use hir_def::{ItemContainerId, Lookup as _, ModuleDefId, signatures::FunctionSignature};
    let mut found: Vec<(String, Vec<(String, bool)>)> = Vec::new();
    for (_, module) in hir_def::nameres::crate_def_map(db, krate).modules(db) {
        let scope = module.scope(db);
        for block in scope.extern_blocks() {
            let libs = AttrFlags::native_libs(db, block);
            let Some(lib) = libs.iter().find(|it| it.kind == NativeLibKind::RawDylib) else {
                continue;
            };
            let mut names = Vec::new();
            for def in scope.declarations() {
                match def {
                    ModuleDefId::FunctionId(id)
                        if id.lookup(db).container == ItemContainerId::ExternBlockId(block) =>
                    {
                        let name = AttrFlags::link_name(db, id.into())
                            .unwrap_or_else(|| FunctionSignature::of(db, id).name.symbol().clone());
                        names.push((name.as_str().to_owned(), false));
                    }
                    ModuleDefId::StaticId(id)
                        if id.lookup(db).container == ItemContainerId::ExternBlockId(block) =>
                    {
                        let name = AttrFlags::link_name(db, id.into()).unwrap_or_else(|| {
                            hir_def::signatures::StaticSignature::of(db, id).name.symbol().clone()
                        });
                        names.push((name.as_str().to_owned(), true));
                    }
                    _ => {}
                }
            }
            let stem = lib.name.as_str().strip_suffix(".dll").unwrap_or(lib.name.as_str());
            match found.iter_mut().find(|(dll, _)| dll == stem) {
                Some((_, known)) => known.extend(names),
                None => found.push((stem.to_owned(), names)),
            }
        }
    }
    found
}

/// What the build script of `krate`'s package printed for the link, as cargo passes it to
/// rustc: `rustc-link-lib`, `rustc-link-search`, `rustc-flags`, `rustc-link-arg`, and
/// `rustc-link-arg-bins` for a binary or `rustc-cdylib-link-arg` for a `cdylib`.
fn asked_by_script(
    db: &RootDatabase,
    krate: base_db::Crate,
    target: &Target,
    cdylib: bool,
) -> Native {
    let mut asked = Native::default();
    let Some(out_dir) = krate.env(db).get("OUT_DIR") else { return asked };
    let printed = crate::cargo::script::printed(Path::new(&out_dir));
    let Some(Ok(printed)) = printed.map(std::fs::read_to_string) else { return asked };
    for line in printed.lines() {
        let Some(line) = line.strip_prefix("cargo:") else { continue };
        let line = line.strip_prefix(':').unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else { continue };
        match key {
            "rustc-link-lib" => asked.libs.extend(lib_of(value)),
            "rustc-link-search" => asked.search.extend(target.search(value)),
            "rustc-link-arg" => asked.args.push(value.to_owned()),
            "rustc-link-arg-bins" if !cdylib => asked.args.push(value.to_owned()),
            "rustc-cdylib-link-arg" | "rustc-link-arg-cdylib" if cdylib => {
                asked.args.push(value.to_owned())
            }
            "rustc-flags" => {
                let mut flags = value.split_whitespace();
                while let Some(flag) = flags.next() {
                    let (kind, rest) = flag.split_at(flag.len().min(2));
                    let rest = if rest.is_empty() { flags.next().unwrap_or("") } else { rest };
                    match kind {
                        "-l" => asked.libs.extend(lib_of(rest)),
                        "-L" => asked.search.extend(target.search(rest)),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    asked
}

/// The library `-l` names: `[KIND[:MODIFIERS]=]NAME[:RENAME]`.
fn lib_of(spec: &str) -> Option<NativeLib> {
    let (kind, name) = match spec.split_once('=') {
        Some((kind, name)) => (Some(kind), name),
        None => (None, spec),
    };
    let (kind, modifiers) = match kind.map(|it| it.split_once(':').unwrap_or((it, ""))) {
        Some((kind, modifiers)) => (Some(kind), modifiers),
        None => (None, ""),
    };
    let kind = match kind {
        None => NativeLibKind::Unspecified,
        Some("static") => NativeLibKind::Static,
        Some("dylib") => NativeLibKind::Dylib,
        Some("framework") => NativeLibKind::Framework,
        Some(_) => return None,
    };
    let name = name.split_once(':').map_or(name, |(_, renamed)| renamed);
    let on = |modifier: &str| modifiers.split(',').any(|it| it.strip_prefix('+') == Some(modifier));
    (!name.is_empty()).then(|| NativeLib {
        name: intern::Symbol::intern(name),
        kind,
        verbatim: on("verbatim"),
        whole_archive: on("whole-archive"),
    })
}

/// Puts `krate` in `order` after every crate it links: what the panic runtime named
/// `unlinked` links is not linked, nor what a proc macro or `test` links unless it is one of
/// the `programs`. A proc macro's library has the macro crate for its program and links libc
/// through it: with libc not named, glibc gives each of its symbols the oldest version, a
/// `realpath` that takes no null buffer.
fn depth_first(
    db: &RootDatabase,
    krate: base_db::Crate,
    unlinked: &str,
    programs: &FxHashSet<base_db::Crate>,
    seen: &mut FxHashSet<base_db::Crate>,
    order: &mut Vec<base_db::Crate>,
) {
    let data = krate.data(db);
    let left_out = (!programs.contains(&krate)
        && (data.target_kind.is_proc_macro()
            || matches!(
                data.origin,
                CrateOrigin::Lang(LangCrateOrigin::ProcMacro | LangCrateOrigin::Test)
            )))
        || Crate::from(krate)
            .display_name(db)
            .is_some_and(|name| name.crate_name().as_str() == unlinked);
    if left_out || !seen.insert(krate) {
        return;
    }
    for dep in &data.dependencies {
        depth_first(db, dep.crate_id, unlinked, programs, seen, order);
    }
    order.push(krate);
}

/// What the link line is made of for the target, from the cfgs of the program's crate.
struct Target {
    apple: bool,
    /// WebAssembly, which `wasm-ld` links with no `cc` in front of it.
    wasm: bool,
    /// `x86_64-pc-windows-msvc`: linked by `link.exe`, not by `cc`.
    windows: bool,
    arch: String,
    os: String,
    /// A shared library is linked, not an executable.
    shared: bool,
}

impl Target {
    fn of(db: &RootDatabase, krate: base_db::Crate) -> Target {
        let cfg = krate.cfg_options(db);
        let value = |key: &str| {
            cfg.get_cfg_values(key).next().map(|it| it.as_str().to_owned()).unwrap_or_default()
        };
        let (apple, arch) = (value("target_vendor") == "apple", value("target_arch"));
        let wasm = value("target_family") == "wasm" || arch.starts_with("wasm");
        let windows = value("target_os") == "windows" && value("target_env") == "msvc";
        Target { apple, wasm, windows, arch, os: value("target_os"), shared: false }
    }

    /// The directory of the C runtime the rust project builds for the target and ships with
    /// the toolchain, which rustc links a WASI program with (`link-self-contained`):
    /// wasi-libc's `crt1-*.o` and `libc.a`.
    fn self_contained(&self, sysroot: &Path) -> Option<PathBuf> {
        let triple = hir_ty_codegen::Target::selected().ok()?.triple();
        let dir = sysroot.join("lib/rustlib").join(triple).join("lib/self-contained");
        dir.is_dir().then_some(dir)
    }

    /// What rustc puts before the objects of a wasm program: the entry of an executable,
    /// exported, the arguments of the target's `pre-link-args`, and the C runtime's start of
    /// a WASI program, whose `_start` (or a library's `_initialize`) calls the entry once the
    /// C library is set up.
    fn wasm_start(&self, sysroot: &Path) -> anyhow::Result<Vec<String>> {
        let mut args = Vec::new();
        if !self.shared && self.os == "wasi" {
            let (entry, _) = hir_ty_codegen::Target::current().entry();
            args.push(format!("--export={entry}"));
        }
        args.extend(
            ["-z", "stack-size=1048576", "--stack-first", "--no-demangle"].map(str::to_owned),
        );
        if self.os != "wasi" {
            args.push("--no-entry".to_owned());
        }
        if self.os == "wasi" {
            let dir = self.self_contained(sysroot).with_context(|| {
                format!(
                    "{} has no wasi-libc for the target (`rustup target add`)",
                    sysroot.display()
                )
            })?;
            let start = if self.shared { "crt1-reactor.o" } else { "crt1-command.o" };
            args.push(dir.join(start).display().to_string());
        }
        Ok(args)
    }

    fn leading(&self) -> Vec<String> {
        if self.wasm {
            return Vec::new();
        }
        if self.windows {
            return ["/NOLOGO", "/DEBUG", "/INCREMENTAL:NO", "/NXCOMPAT", "/OPT:REF,NOICF"]
                .map(str::to_owned)
                .to_vec();
        }
        if self.apple {
            let arch = if self.arch == "aarch64" { "arm64" } else { &self.arch };
            // Edits are spliced into what this links, and the splice reads fixups in tables.
            ["-arch", arch, crate::cargo::plan::NO_FIXUP_CHAINS].map(str::to_owned).to_vec()
        } else {
            let width = match &*self.arch {
                "x86_64" => Some("-m64"),
                "x86" => Some("-m32"),
                _ => None,
            };
            // A program that takes edits while it runs is known by its build ID.
            let args = ["-Wl,--as-needed", "-Wl,--build-id"];
            width.into_iter().chain(args).map(str::to_owned).collect()
        }
    }

    fn lib(&self, lib: &NativeLib) -> Vec<String> {
        let name = lib.name.as_str();
        if self.wasm {
            // `wasm-ld` links archives alone, and has no `-B`.
            let l = if lib.verbatim { format!("-l:{name}") } else { format!("-l{name}") };
            return match lib.kind {
                NativeLibKind::Static if lib.whole_archive => {
                    vec!["--whole-archive".to_owned(), l, "--no-whole-archive".to_owned()]
                }
                _ => vec![l],
            };
        }
        if self.windows {
            // A `raw-dylib` is linked by the import library made of it, whatever it is named:
            // `windows-link` names the DLLs of the API sets with `+verbatim` and the extension.
            let stem = name.strip_suffix(".dll").unwrap_or(name);
            let file = match (lib.kind, lib.verbatim) {
                (NativeLibKind::RawDylib, _) => format!("{stem}.lib"),
                (_, true) => name.to_owned(),
                _ => format!("{name}.lib"),
            };
            return match lib.kind {
                NativeLibKind::Static if lib.whole_archive => vec![format!("/WHOLEARCHIVE:{file}")],
                _ => vec![file],
            };
        }
        if self.apple {
            return match lib.kind {
                NativeLibKind::Framework => vec!["-framework".to_owned(), name.to_owned()],
                _ if lib.verbatim => vec![name.to_owned()],
                NativeLibKind::Static if lib.whole_archive => {
                    vec![format!("-Wl,-force_load,lib{name}.a")]
                }
                _ => vec![format!("-l{name}")],
            };
        }
        let l = if lib.verbatim { format!("-l:{name}") } else { format!("-l{name}") };
        match lib.kind {
            NativeLibKind::Static if lib.whole_archive => [
                "-Wl,-Bstatic",
                "-Wl,--whole-archive",
                &l,
                "-Wl,--no-whole-archive",
                "-Wl,-Bdynamic",
            ]
            .map(str::to_owned)
            .to_vec(),
            NativeLibKind::Static => {
                ["-Wl,-Bstatic", &l, "-Wl,-Bdynamic"].map(str::to_owned).to_vec()
            }
            _ => vec![l],
        }
    }

    /// Where a script's `rustc-link-search=[KIND=]PATH` has the linker look: rustc gives it
    /// the native and framework paths, not those it finds crates in.
    fn search(&self, spec: &str) -> Option<String> {
        let (kind, path) = match spec.split_once('=') {
            Some((kind @ ("dependency" | "crate" | "native" | "framework" | "all"), path)) => {
                (kind, path)
            }
            _ => ("all", spec),
        };
        match kind {
            "native" | "all" if self.windows => Some(format!("/LIBPATH:{path}")),
            "native" | "all" => Some(format!("-L{path}")),
            "framework" => Some(if self.apple { format!("-F{path}") } else { format!("-L{path}") }),
            _ => None,
        }
    }

    /// The rest of rustc's arguments: what is not reached is left out, and nothing is linked
    /// that the crates did not ask for.
    fn trailing(&self) -> Vec<String> {
        if self.wasm {
            let mut args = vec!["--gc-sections".to_owned()];
            if self.shared {
                args.push("--no-entry".to_owned());
            }
            return args;
        }
        if self.windows {
            // The dynamic C runtime, which rustc's objects name with `/DEFAULTLIB` and
            // blaze's do not.
            return ["msvcrt.lib", "vcruntime.lib", "ucrt.lib", "oldnames.lib"]
                .map(str::to_owned)
                .to_vec();
        }
        let args: &[&str] = match (self.apple, self.shared) {
            (true, false) => &["-Wl,-dead_strip", "-nodefaultlibs"],
            (true, true) => &["-dynamiclib", "-Wl,-dead_strip", "-nodefaultlibs"],
            (false, false) => &[
                "-Wl,--eh-frame-hdr",
                "-Wl,-z,noexecstack",
                "-Wl,--gc-sections",
                "-pie",
                "-Wl,-z,relro,-z,now",
                "-nodefaultlibs",
            ],
            // A call in the library goes to its own definition, which the code generator
            // took for one in the same object: nothing loaded before it may stand in.
            (false, true) => &[
                "-Wl,--eh-frame-hdr",
                "-Wl,-z,noexecstack",
                "-Wl,--gc-sections",
                "-shared",
                "-Wl,-Bsymbolic",
                "-Wl,-z,relro,-z,now",
                "-nodefaultlibs",
            ],
        };
        args.iter().map(|it| (*it).to_owned()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn windows_target() -> Target {
        Target {
            apple: false,
            wasm: false,
            windows: true,
            arch: "x86_64".to_owned(),
            os: "windows".to_owned(),
            shared: false,
        }
    }

    /// The arguments of `link.exe` for a native library are its own: a `.lib` to name, a
    /// `/WHOLEARCHIVE` to take an archive whole, a `/LIBPATH` to look in.
    #[test]
    fn a_native_library_of_a_windows_program_is_named_as_link_exe_takes_it() {
        let target = windows_target();
        let lib = |spec| target.lib(&lib_of(spec).unwrap());
        assert_eq!(lib("kernel32"), ["kernel32.lib"]);
        assert_eq!(lib("static:+whole-archive=ring"), ["/WHOLEARCHIVE:ring.lib"]);
        assert_eq!(lib("dylib:+verbatim=x.lib"), ["x.lib"]);
        assert_eq!(target.search("native=C:\\sdk\\lib").as_deref(), Some("/LIBPATH:C:\\sdk\\lib"));
        assert_eq!(target.search("framework=F"), Some("-LF".to_owned()));
        assert_eq!(target.leading()[0], "/NOLOGO");
        assert_eq!(target.trailing(), ["msvcrt.lib", "vcruntime.lib", "ucrt.lib", "oldnames.lib"]);
    }

    /// The import library of a `raw-dylib` is made from the names imported, unless the system
    /// has one; a `raw-dylib` is named by its `.lib` whatever extension the attribute gives.
    #[cfg(windows)]
    #[test]
    fn the_import_library_of_a_raw_dylib_is_made_of_the_names_imported() {
        let dir = std::env::temp_dir().join(format!("blaze-implib-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let line = Line {
            program: PathBuf::from("link.exe"),
            before: Vec::new(),
            after: Vec::new(),
            apple: false,
            wasm: false,
            windows: true,
            raw_dylibs: vec![(
                "api-ms-win-core-test-l1-1-0".to_owned(),
                vec![("Function".to_owned(), false), ("Data".to_owned(), true)],
            )],
        };
        let exe = dir.join("program.exe");
        let made = line.import_libraries(&[], &exe).unwrap().unwrap();
        let lib = made.join("api-ms-win-core-test-l1-1-0.lib");
        assert!(lib.is_file());
        let def = std::fs::read_to_string(made.join("api-ms-win-core-test-l1-1-0.def")).unwrap();
        assert!(def.contains("LIBRARY api-ms-win-core-test-l1-1-0.dll"));
        assert!(def.contains("  Function\n") && def.contains("  Data DATA\n"));
        // One the system's directories have is not made again.
        assert!(line.import_libraries(&[made], &exe).unwrap().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_raw_dylib_is_linked_by_its_import_library_whatever_its_extension() {
        let dll = NativeLib {
            name: intern::Symbol::intern("api-ms-win-core-test-l1-1-0.dll"),
            kind: NativeLibKind::RawDylib,
            verbatim: true,
            whole_archive: false,
        };
        assert_eq!(windows_target().lib(&dll), ["api-ms-win-core-test-l1-1-0.lib"]);
    }

    #[test]
    fn a_scripts_libraries_are_read_as_rustc_reads_them() {
        let lib = |spec| {
            lib_of(spec)
                .map(|it| (it.name.as_str().to_owned(), it.kind, it.verbatim, it.whole_archive))
        };
        assert_eq!(
            lib("sqlite3"),
            Some(("sqlite3".to_owned(), NativeLibKind::Unspecified, false, false))
        );
        assert_eq!(
            lib("static=sqlite3"),
            Some(("sqlite3".to_owned(), NativeLibKind::Static, false, false))
        );
        assert_eq!(
            lib("static:+whole-archive,-bundle=foo:bar"),
            Some(("bar".to_owned(), NativeLibKind::Static, false, true))
        );
        assert_eq!(
            lib("dylib:+verbatim=libz.so.1"),
            Some(("libz.so.1".to_owned(), NativeLibKind::Dylib, true, false))
        );
        assert_eq!(lib("bogus=x"), None);
        let linux = Target {
            apple: false,
            wasm: false,
            windows: false,
            arch: "x86_64".to_owned(),
            os: "linux".to_owned(),
            shared: false,
        };
        assert_eq!(linux.search("native=/a/out").as_deref(), Some("-L/a/out"));
        assert_eq!(linux.search("/b").as_deref(), Some("-L/b"));
        assert_eq!(linux.search("dependency=/c"), None);
    }

    #[test]
    fn an_apple_link_has_fixups_in_tables_for_the_splice() {
        for shared in [false, true] {
            let mac = Target {
                apple: true,
                wasm: false,
                windows: false,
                arch: "aarch64".to_owned(),
                os: "macos".to_owned(),
                shared,
            };
            assert_eq!(mac.leading(), ["-arch", "arm64", "-Wl,-no_fixup_chains"]);
        }
    }

    #[test]
    fn a_wasm_modules_stand_ins_are_the_segment_named_for_them() {
        let (_, name) = hir_ty_codegen::stand_in_section(object::BinaryFormat::Wasm);
        let said = format!("{} f: unsupported", hir_ty_codegen::STAND_IN_SAYS);
        let message = format!("{said}\n\0\0").into_bytes();
        let module = |named: bool| {
            let mut data = wasm_encoder::DataSection::new();
            data.passive(*b"what the program says\n");
            data.passive(message.iter().copied());
            let mut module = wasm_encoder::Module::new();
            module.section(&data);
            if named {
                let mut names = wasm_encoder::NameMap::new();
                names.append(0, ".rodata");
                names.append(1, name);
                let mut section = wasm_encoder::NameSection::new();
                section.data(&names);
                module.section(&section);
            }
            module.finish()
        };
        let named = module(true);
        let segments = wasm_segments_named(&named, name).unwrap();
        assert_eq!(messages(segments, name, Path::new("m.wasm")).unwrap(), [said]);
        assert!(wasm_segments_named(&module(false), name).is_err());
    }
}
