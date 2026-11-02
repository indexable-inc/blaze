use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use blaze_codegen::settings::Sanitizers;
use blaze_codegen::{Arch, NativeLib, NativeLibKind};
use object::write::Object as Builder;
use object::{Architecture, BinaryFormat, Endianness, SectionKind};

use super::{
    Ask, Cache, Located, Target, caseless, fhs, listing, loader, locate_with, output_with, parse_version, rpaths,
    top_level_string,
};
use crate::{Debug, Format, LinkError, NativeKind, Output, OutputKind, Platform, Searched};

/// A machine that answers what it is told to: variables, and commands by their whole text.
struct Answers {
    vars: Vec<(&'static str, String)>,
    runs: Vec<(String, Result<String, String>)>,
    asked: RefCell<Vec<String>>,
}

impl Answers {
    fn new() -> Answers {
        Answers { vars: Vec::new(), runs: Vec::new(), asked: RefCell::new(Vec::new()) }
    }

    fn var(mut self, name: &'static str, value: impl Into<String>) -> Answers {
        self.vars.push((name, value.into()));
        self
    }

    fn answer(mut self, command: impl Into<String>, printed: Result<String, String>) -> Answers {
        self.runs.push((command.into(), printed));
        self
    }

    fn asked(&self) -> usize {
        self.asked.borrow().len()
    }
}

impl Ask for Answers {
    fn var(&self, name: &str) -> Result<Option<String>, LinkError> {
        Ok(self.vars.iter().find(|it| it.0 == name).map(|it| it.1.clone()))
    }

    fn run(&self, program: &Path, runs: &[Vec<String>]) -> Vec<Result<String, String>> {
        runs.iter()
            .map(|args| {
                let command = format!("{} {}", program.display(), args.join(" "));
                self.asked.borrow_mut().push(command.clone());
                self.runs
                    .iter()
                    .find(|it| it.0 == command)
                    .map_or_else(|| Err("No such file or directory (os error 2)".to_owned()), |it| it.1.clone())
            })
            .collect()
    }

    fn said(&self, program: &Path, args: &[String]) -> Result<String, String> {
        let mut answers = self.run(program, &[args.to_vec()]);
        answers.pop().unwrap()
    }
}

/// How a C driver is asked for its linker's job.
const DRIVER: &str = "cc -### -x c /dev/null";

/// What a C driver's `-###` prints: its version, a compile job and its linker's job, which
/// passes `dirs` with `-L` (the first as a separate word).
fn jobs(dirs: &[&Path]) -> Result<String, String> {
    let mut printed = "clang version 21.0.0\nTarget: arm64-apple-darwin\n \"/usr/bin/clang\" \"-cc1\" \"-o\" \"x.o\"\n".to_owned();
    printed.push_str(r#" "/usr/bin/ld" "-demangle" "-o" "a.out""#);
    for (at, dir) in dirs.iter().enumerate() {
        match at {
            0 => printed.push_str(&format!(r#" "-L" "{}""#, dir.display())),
            _ => printed.push_str(&format!(r#" "-L{}""#, dir.display())),
        }
    }
    printed.push_str(" \"-L.\" \"-Lrelative/lib\" \"x.o\" \"-lSystem\"\n");
    Ok(printed)
}

fn temp(name: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "blaze-link-target-{}-{name}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A `.tbd` for AArch64 and x86-64 macOS whose install name is `install`, re-exporting the
/// libraries `reexports`.
fn stub(install: &str, reexports: &[&str]) -> Vec<u8> {
    let mut text = format!("--- !tapi-tbd\ntbd-version: 4\ntargets: [ x86_64-macos, arm64-macos ]\ninstall-name: '{install}'\n");
    if !reexports.is_empty() {
        let libraries: Vec<String> = reexports.iter().map(|it| format!("'{it}'")).collect();
        text.push_str(&format!(
            "reexported-libraries:\n  - targets: [ x86_64-macos, arm64-macos ]\n    libraries: [ {} ]\n",
            libraries.join(", ")
        ));
    }
    text.push_str("...\n");
    text.into_bytes()
}

fn touch(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// The first bytes of a 64-bit little-endian ELF file of `e_type`.
fn elf(e_type: u16) -> Vec<u8> {
    let mut bytes = vec![0; 64];
    bytes[..4].copy_from_slice(b"\x7fELF");
    bytes[4] = 2;
    bytes[5] = 1;
    bytes[6] = 1;
    bytes[16..18].copy_from_slice(&e_type.to_le_bytes());
    bytes
}

const ARCHIVE: &[u8] = b"!<arch>\n";

/// A glibc toolchain outside the FHS directories, as nix has one: GCC's files in `gcc`,
/// glibc's in `libc` with its `libc.so` script, and a `cc` that says where they are.
struct Glibc {
    gcc: PathBuf,
    libc: PathBuf,
    loader: &'static str,
    answers: Answers,
}

fn glibc(target: Target) -> Glibc {
    let root = temp("glibc");
    let gcc = root.join("gcc");
    let libc = root.join("libc");
    let (loader, _, _) = loader(target);
    for (dir, file) in [(&libc, "Scrt1.o"), (&libc, "crti.o"), (&libc, "crtn.o"), (&gcc, "crtbeginS.o"), (&gcc, "crtendS.o")] {
        touch(&dir.join(file), &elf(1));
    }
    touch(&gcc.join("libgcc.a"), ARCHIVE);
    touch(&gcc.join("libgcc_s.so"), &elf(3));
    touch(&libc.join("libc.so.6"), &elf(3));
    touch(&libc.join(loader), &elf(3));
    touch(&libc.join("libc_nonshared.a"), ARCHIVE);
    let script = format!(
        "/* GNU ld script\n   Use the shared library, but some functions are only in\n   the static library. */\n\
         OUTPUT_FORMAT(elf64-x86-64)\nGROUP ( {} libc_nonshared.a  AS_NEEDED ( {} ) )\n",
        libc.join("libc.so.6").display(),
        libc.join(loader).display()
    );
    touch(&libc.join("libc.so"), script.as_bytes());
    let mut answers = Answers::new().answer(
        "cc -print-search-dirs",
        Ok(format!(
            "install: {gcc}/\nprograms: =/usr/bin/\nlibraries: ={gcc}/:{libc}/:{missing}\n",
            gcc = gcc.display(),
            libc = libc.display(),
            missing = root.join("missing").display()
        )),
    );
    let files = [
        (&libc, "Scrt1.o"),
        (&libc, "crti.o"),
        (&gcc, "crtbeginS.o"),
        (&gcc, "crtendS.o"),
        (&libc, "crtn.o"),
        (&libc, "libc.so.6"),
        (&libc, loader),
    ];
    for (dir, file) in files {
        answers = answers.answer(format!("cc -print-file-name={file}"), Ok(format!("{}\n", dir.join(file).display())));
    }
    let answers = answers.answer(DRIVER, jobs(&[]));
    Glibc { gcc, libc, loader, answers }
}

fn dylib(name: &str) -> NativeLib {
    NativeLib { name: name.to_owned(), kind: NativeLibKind::Dylib, verbatim: false }
}

fn static_lib(name: &str, whole: bool) -> NativeLib {
    NativeLib { name: name.to_owned(), kind: NativeLibKind::Static { whole, bundle: true }, verbatim: false }
}

fn verbatim(name: &str) -> NativeLib {
    NativeLib { name: name.to_owned(), kind: NativeLibKind::Dylib, verbatim: true }
}

fn executable(cache: &Cache, answers: &Answers, triple: &str) -> Result<Output, LinkError> {
    output_with(cache, answers, triple, None, Debug::None, true)
}

fn shared(cache: &Cache, answers: &Answers, triple: &str) -> Result<Output, LinkError> {
    output_with(cache, answers, triple, Some(("libdemo.so".to_owned(), vec!["demo".to_owned()])), Debug::Lines, false)
}

/// [`locate_with`] for a target that links nothing from blaze's toolchain.
fn locate_plain(
    cache: &Cache,
    answers: &Answers,
    output: &Output,
    libs: &[NativeLib],
    search: &[PathBuf],
) -> Result<Vec<Located>, LinkError> {
    locate_with(cache, answers, output, libs, search, Path::new("/no-blaze-toolchain"))
}

fn line(located: &[Located]) -> Vec<(PathBuf, NativeKind)> {
    for it in located {
        assert_eq!(it.name, it.path.to_string_lossy(), "a file's name is its path");
    }
    located.iter().map(|it| (it.path.clone(), it.kind)).collect()
}

const SHARED: NativeKind = NativeKind::Shared { as_needed: true };
const ARCHIVED: NativeKind = NativeKind::Archive { whole: false };

#[test]
fn a_linux_executable_is_entered_at_start_and_interpreted_by_its_libcs_loader() {
    for (triple, target, arch) in [
        ("x86_64-unknown-linux-gnu", Target::LinuxX86_64, Arch::X86_64),
        ("aarch64-unknown-linux-gnu", Target::LinuxAarch64, Arch::Aarch64),
    ] {
        let glibc = glibc(target);
        let output = executable(&Cache::new(), &glibc.answers, triple).unwrap();
        assert_eq!((output.format, output.arch), (Format::Elf, arch));
        assert_eq!(output.kind, OutputKind::Executable { entry: "_start".to_owned() });
        let loader = glibc.libc.join(glibc.loader);
        assert_eq!(output.interpreter.as_deref(), loader.to_str(), "nix's libc is not the FHS one");
        assert_eq!((output.platform, output.stack), (None, None));
        assert_eq!(output.triple, triple);
        assert!(output.incremental && output.live.is_none() && output.dev_run.is_none());
        assert!(!output.windows_subsystem && output.tags == 0);
    }
}

#[test]
fn the_glibc_abis_loader_is_the_interpreter_where_libc_is_the_fhs_ones() {
    assert_eq!(loader(Target::LinuxX86_64).1, "/lib64/ld-linux-x86-64.so.2");
    assert_eq!(loader(Target::LinuxAarch64).1, "/lib/ld-linux-aarch64.so.1");
    for dir in ["/lib", "/lib64", "/usr/lib", "/usr/lib64/", "/usr/lib/x86_64-linux-gnu", "/lib/x86_64-linux-gnu"] {
        assert!(fhs(Path::new(dir), "x86_64-linux-gnu"), "{dir}");
    }
    for dir in ["/nix/store/abc-glibc-2.40/lib", "/usr/lib/aarch64-linux-gnu", "/opt/lib", "/usr/local/lib"] {
        assert!(!fhs(Path::new(dir), "x86_64-linux-gnu"), "{dir}");
    }
}

#[test]
fn a_shared_library_has_its_name_and_exports_and_asks_no_toolchain() {
    let answers = Answers::new();
    let output = shared(&Cache::new(), &answers, "x86_64-unknown-linux-gnu").unwrap();
    assert_eq!(output.kind, OutputKind::Shared { name: "libdemo.so".to_owned(), exports: vec!["demo".to_owned()] });
    assert_eq!((output.interpreter, output.debug, output.incremental), (None, Debug::Lines, false));
    assert_eq!(answers.asked(), 0);
}

#[test]
fn a_macos_executable_says_its_sdk_and_the_minimum_os() {
    let sdk = temp("sdk");
    touch(
        &sdk.join("SDKSettings.json"),
        br#"{"CanonicalName":"macosx15.2","VersionMap":{"Version":"1.0","macOS_iOSMac":{"15.2":"18.2"}},"Version":"15.2","MinimalDisplayName":"15.2"}"#,
    );
    let answers = Answers::new().var("SDKROOT", sdk.display().to_string()).answer(DRIVER, jobs(&[]));
    let output = executable(&Cache::new(), &answers, "aarch64-apple-darwin").unwrap();
    assert_eq!((output.format, output.arch), (Format::MachO, Arch::Aarch64));
    assert_eq!(output.kind, OutputKind::Executable { entry: "main".to_owned() });
    assert_eq!(output.platform, Some(Platform { platform: 1, minimum: 0x000b_0000, sdk: 0x000f_0200 }));
    assert_eq!((output.interpreter, output.stack), (None, None));

    for (given, minimum) in [("14.5", 0x000e_0500), ("10.9", 0x000b_0000), ("26", 0x001a_0000), ("15.1.2", 0x000f_0102)] {
        let answers = Answers::new()
            .var("SDKROOT", sdk.display().to_string())
            .var("MACOSX_DEPLOYMENT_TARGET", given)
            .answer(DRIVER, jobs(&[]));
        let output = executable(&Cache::new(), &answers, "aarch64-apple-darwin").unwrap();
        assert_eq!(output.platform.unwrap().minimum, minimum, "{given}");
    }
    let answers = Answers::new()
        .var("SDKROOT", sdk.display().to_string())
        .var("MACOSX_DEPLOYMENT_TARGET", "eleven")
        .answer(DRIVER, jobs(&[]));
    let error = executable(&Cache::new(), &answers, "aarch64-apple-darwin").unwrap_err();
    assert!(error.what.contains("MACOSX_DEPLOYMENT_TARGET=eleven"), "{}", error.what);
}

#[test]
fn without_sdkroot_the_sdk_is_where_xcrun_says() {
    let sdk = temp("xcrun");
    touch(&sdk.join("SDKSettings.json"), br#"{"Version": "14.0"}"#);
    let answers = Answers::new()
        .answer("xcrun --sdk macosx --show-sdk-path", Ok(format!("{}\n", sdk.display())))
        .answer(DRIVER, jobs(&[]));
    let output = executable(&Cache::new(), &answers, "aarch64-apple-darwin").unwrap();
    assert_eq!(output.platform.unwrap().sdk, 0x000e_0000);

    let error = executable(&Cache::new(), &Answers::new(), "aarch64-apple-darwin").unwrap_err();
    assert!(error.what.contains("SDKROOT is not set") && error.what.contains("xcrun"), "{}", error.what);
}

#[test]
fn a_windows_executable_enters_the_crt_with_rustcs_stack() {
    let output = executable(&Cache::new(), &Answers::new(), "x86_64-pc-windows-msvc").unwrap();
    assert_eq!((output.format, output.arch), (Format::Pe, Arch::X86_64));
    assert_eq!(output.kind, OutputKind::Executable { entry: "mainCRTStartup".to_owned() });
    assert_eq!((output.stack, output.interpreter, output.platform), (Some(1 << 20), None, None));
}

#[test]
fn a_wasi_command_starts_at_start_and_wasm32_unknown_unknown_links_only_libraries() {
    let output = executable(&Cache::new(), &Answers::new(), "wasm32-wasip1").unwrap();
    assert_eq!((output.format, output.arch), (Format::Wasm, Arch::Wasm32));
    assert_eq!(output.kind, OutputKind::Executable { entry: "_start".to_owned() });
    assert_eq!(output.stack, Some(1 << 20));

    let error = executable(&Cache::new(), &Answers::new(), "wasm32-unknown-unknown").unwrap_err();
    assert!(error.what.contains("--no-entry"), "{}", error.what);
    let output = shared(&Cache::new(), &Answers::new(), "wasm32-unknown-unknown").unwrap();
    assert!(matches!(output.kind, OutputKind::Shared { .. }));
}

#[test]
fn another_triple_is_an_error_naming_it() {
    for triple in ["x86_64-apple-darwin", "x86_64-unknown-linux-musl", "aarch64-pc-windows-msvc", "riscv64gc-unknown-linux-gnu"] {
        let error = executable(&Cache::new(), &Answers::new(), triple).unwrap_err();
        assert!(error.what.contains(triple) && error.what.contains("x86_64-unknown-linux-gnu"), "{}", error.what);
    }
    let error = executable(&Cache::new(), &Answers::new(), "wasm32-wasip2").unwrap_err();
    assert!(error.what.contains("component"), "{}", error.what);
}

#[test]
fn a_linux_line_is_the_c_runtime_around_the_libraries_with_libcs_script_followed() {
    let glibc = glibc(Target::LinuxX86_64);
    let cache = Cache::new();
    let output = executable(&cache, &glibc.answers, "x86_64-unknown-linux-gnu").unwrap();
    let mine = temp("mine");
    touch(&mine.join("libfoo.a"), ARCHIVE);
    let libs = vec![static_lib("foo", true), dylib("gcc_s"), dylib("c")];
    let search = vec![mine.clone()];
    let located = locate_plain(&cache, &glibc.answers, &output, &libs, &search).unwrap();
    let (gcc, libc) = (&glibc.gcc, &glibc.libc);
    assert_eq!(
        line(&located),
        [
            (libc.join("Scrt1.o"), NativeKind::Object),
            (libc.join("crti.o"), NativeKind::Object),
            (gcc.join("crtbeginS.o"), NativeKind::Object),
            (mine.join("libfoo.a"), NativeKind::Archive { whole: true }),
            (gcc.join("libgcc_s.so"), SHARED),
            (libc.join("libc.so"), SHARED),
            (libc.join("libc.so.6"), SHARED),
            (libc.join("libc_nonshared.a"), ARCHIVED),
            (libc.join(glibc.loader), SHARED),
            (gcc.join("libgcc.a"), ARCHIVED),
            (gcc.join("crtendS.o"), NativeKind::Object),
            (libc.join("crtn.o"), NativeKind::Object),
        ]
    );

    let output = shared(&cache, &glibc.answers, "x86_64-unknown-linux-gnu").unwrap();
    let located = locate_plain(&cache, &glibc.answers, &output, &[], &[]).unwrap();
    let first: Vec<PathBuf> = located.iter().take(2).map(|it| it.path.clone()).collect();
    assert_eq!(first, [libc.join("crti.o"), gcc.join("crtbeginS.o")], "a shared library has no Scrt1.o");
}

const DEBIAN_GCC_S: &[u8] =
    b"/* GNU ld script\n   Use the shared library, but some functions are only in\n   the static library.  */\nGROUP ( libgcc_s.so.1 -lgcc )\n";

#[test]
fn debians_libgcc_s_script_is_followed_by_libgcc_s_so_1_and_libgcc() {
    let glibc = glibc(Target::LinuxX86_64);
    let (gcc, libc) = (&glibc.gcc, &glibc.libc);
    touch(&gcc.join("libgcc_s.so"), DEBIAN_GCC_S);
    touch(&libc.join("libgcc_s.so.1"), &elf(3));
    let cache = Cache::new();
    let output = executable(&cache, &glibc.answers, "x86_64-unknown-linux-gnu").unwrap();
    let located = locate_plain(&cache, &glibc.answers, &output, &[dylib("gcc_s")], &[]).unwrap();
    assert_eq!(
        line(&located)[3..6].to_vec(),
        [(gcc.join("libgcc_s.so"), SHARED), (libc.join("libgcc_s.so.1"), SHARED), (gcc.join("libgcc.a"), ARCHIVED)]
    );
    assert_eq!(located.iter().filter(|it| it.path == gcc.join("libgcc.a")).count(), 1, "the late -lgcc is on the line once");

    touch(&gcc.join("libgcc_s.so"), b"GROUP ( libgcc_s.so.1 -lnope )");
    let error = locate_plain(&Cache::new(), &glibc.answers, &output, &[dylib("gcc_s")], &[]).unwrap_err();
    for said in ["libgcc_s.so names -lnope", "libnope.so, libnope.a"] {
        assert!(error.what.contains(said), "{}", error.what);
    }
    assert_eq!(error.not_found().map(|it| it.library.as_str()), Some("nope"), "the script's context keeps it typed");

    touch(&gcc.join("libgcc_s.so"), b"GROUP ( -l:libgcc_s.so.1 )");
    let located = locate_plain(&cache, &glibc.answers, &output, &[dylib("gcc_s")], &[]).unwrap();
    assert_eq!(line(&located)[4], (libc.join("libgcc_s.so.1"), SHARED), "-l:FILE is the file");
}

#[test]
fn a_library_is_the_first_file_the_kind_allows_in_the_first_directory_that_has_one() {
    let glibc = glibc(Target::LinuxX86_64);
    let cache = Cache::new();
    let output = executable(&cache, &glibc.answers, "x86_64-unknown-linux-gnu").unwrap();
    let (a, b) = (temp("a"), temp("b"));
    touch(&a.join("libx.a"), ARCHIVE);
    touch(&b.join("libx.so"), &elf(3));
    touch(&a.join("liby.so"), &elf(3));
    touch(&a.join("liby.a"), ARCHIVE);
    touch(&b.join("z.o"), &elf(1));
    touch(&b.join("libw.so.1"), &elf(3));
    touch(&b.join("libv.a"), ARCHIVE);
    std::fs::create_dir_all(a.join("libq.so")).unwrap();
    touch(&b.join("libq.so"), &elf(3));
    let search = vec![a.clone(), b.clone()];
    let mine = |libs: &[NativeLib]| -> Vec<(PathBuf, NativeKind)> {
        let located = locate_plain(&cache, &glibc.answers, &output, libs, &search).unwrap();
        line(&located)[3..3 + libs.len()].to_vec()
    };
    assert_eq!(mine(&[dylib("x")]), [(a.join("libx.a"), ARCHIVED)], "the first directory wins over the kind");
    assert_eq!(mine(&[dylib("y")]), [(a.join("liby.so"), SHARED)], "a dylib is .so before .a");
    assert_eq!(mine(&[static_lib("y", false)]), [(a.join("liby.a"), ARCHIVED)]);
    assert_eq!(mine(&[static_lib("v", true)]), [(b.join("libv.a"), NativeKind::Archive { whole: true })]);
    assert_eq!(mine(&[verbatim("z.o")]), [(b.join("z.o"), NativeKind::Object)]);
    assert_eq!(mine(&[verbatim("libw.so.1")]), [(b.join("libw.so.1"), SHARED)]);
    assert_eq!(mine(&[dylib("q")]), [(b.join("libq.so"), SHARED)], "a directory listed with the name is not a file");
    assert_eq!(mine(&[dylib("gcc")]), [(glibc.gcc.join("libgcc.a"), ARCHIVED)], "then the toolchain's directories");

    let error = locate_plain(&cache, &glibc.answers, &output, &[dylib("nope")], &search).unwrap_err();
    for said in ["`nope`", "libnope.so, libnope.a"] {
        assert!(error.what.contains(said), "{}", error.what);
    }
    for dir in [&a, &b, &glibc.gcc, &glibc.libc] {
        assert!(error.what.contains(dir.to_str().unwrap()), "{}", error.what);
    }
    let framework = NativeLib { name: "Cocoa".to_owned(), kind: NativeLibKind::Framework, verbatim: false };
    let error = locate_plain(&cache, &glibc.answers, &output, &[framework], &search).unwrap_err();
    assert!(error.what.contains("Cocoa") && error.what.contains("Apple"), "{}", error.what);
}

#[test]
fn a_search_directory_is_listed_with_its_names_case_folded() {
    let dir = temp("listing");
    touch(&dir.join("LibFoo.A"), ARCHIVE);
    std::fs::create_dir_all(dir.join("Sub.framework")).unwrap();
    let names = listing(&dir).unwrap();
    assert!(names.contains(&b"libfoo.a"[..]) && names.contains(&b"sub.framework"[..]), "{names:?}");
    assert_eq!(names.len(), 2);
    assert!(listing(&dir.join("missing")).is_none(), "a directory that can't be listed leaves its names to a stat");
}

#[test]
fn the_toolchain_is_asked_once_per_environment() {
    let glibc = glibc(Target::LinuxX86_64);
    let cache = Cache::new();
    executable(&cache, &glibc.answers, "x86_64-unknown-linux-gnu").unwrap();
    let asked = glibc.answers.asked();
    assert_eq!(asked, 9, "the search directories, seven files and the linker's job");
    executable(&cache, &glibc.answers, "x86_64-unknown-linux-gnu").unwrap();
    locate_plain(&cache, &glibc.answers, &executable(&cache, &glibc.answers, "x86_64-unknown-linux-gnu").unwrap(), &[], &[])
        .unwrap();
    assert_eq!(glibc.answers.asked(), asked);

    let other = Answers::new().var("CC", "gcc-14 -m64");
    let error = executable(&cache, &other, "x86_64-unknown-linux-gnu").unwrap_err();
    assert!(other.asked.borrow()[0] == "gcc-14 -m64 -print-search-dirs", "{:?}", other.asked.borrow());
    assert!(error.what.contains("`gcc-14 -m64`, from CC"), "{}", error.what);
}

#[test]
fn no_c_toolchain_is_an_error_naming_what_was_tried() {
    let error = executable(&Cache::new(), &Answers::new(), "x86_64-unknown-linux-gnu").unwrap_err();
    for said in ["`cc -print-search-dirs`", "No such file", "neither CC_x86_64_unknown_linux_gnu nor CC is set"] {
        assert!(error.what.contains(said), "{}", error.what);
    }
    let answers = Answers::new()
        .var("CC_aarch64_unknown_linux_gnu", "aarch64-linux-gnu-gcc")
        .answer("aarch64-linux-gnu-gcc -print-search-dirs", Ok("libraries: =/nowhere\n".to_owned()))
        .answer("aarch64-linux-gnu-gcc -print-file-name=Scrt1.o", Ok("Scrt1.o\n".to_owned()));
    let error = executable(&Cache::new(), &answers, "aarch64-unknown-linux-gnu").unwrap_err();
    for said in ["did not find Scrt1.o", "from CC_aarch64_unknown_linux_gnu"] {
        assert!(error.what.contains(said), "{}", error.what);
    }
}

#[test]
fn a_macos_line_takes_tbds_before_dylibs_and_frameworks_from_the_sdk() {
    let sdk = temp("sdk-line");
    touch(&sdk.join("SDKSettings.json"), br#"{"Version":"15.0"}"#);
    let tbd = &stub("/usr/lib/libstub.dylib", &[]);
    touch(&sdk.join("usr/lib/libSystem.tbd"), tbd);
    touch(&sdk.join("usr/lib/libiconv.tbd"), tbd);
    touch(&sdk.join("usr/lib/libiconv.dylib"), tbd);
    touch(&sdk.join("System/Library/Frameworks/CoreFoundation.framework/CoreFoundation.tbd"), tbd);
    let answers = Answers::new().var("SDKROOT", sdk.display().to_string()).answer(DRIVER, jobs(&[]));
    let cache = Cache::new();
    let output = executable(&cache, &answers, "aarch64-apple-darwin").unwrap();
    let framework = NativeLib { name: "CoreFoundation".to_owned(), kind: NativeLibKind::Framework, verbatim: false };
    let located = locate_plain(&cache, &answers, &output, &[dylib("iconv"), framework], &[]).unwrap();
    let shared = NativeKind::Shared { as_needed: false };
    assert_eq!(
        line(&located),
        [
            (sdk.join("usr/lib/libiconv.tbd"), shared),
            (sdk.join("System/Library/Frameworks/CoreFoundation.framework/CoreFoundation.tbd"), shared),
            (sdk.join("usr/lib/libSystem.tbd"), shared),
        ]
    );
}

/// nix's dev shell on macOS: libiconv is not in the SDK, and cc-wrapper passes its directory
/// to the linker from `NIX_LDFLAGS`, as rustc's link through `cc` gets it.
#[test]
fn a_library_only_on_a_path_the_c_driver_passes_its_linker_is_found() {
    let sdk = temp("sdk-driver");
    touch(&sdk.join("SDKSettings.json"), br#"{"Version":"15.0"}"#);
    touch(&sdk.join("usr/lib/libSystem.tbd"), &stub("/usr/lib/libSystem.B.dylib", &[]));
    let (nix, profile, mine) = (temp("nix-libiconv"), temp("nix-profile"), temp("mine"));
    touch(&nix.join("libiconv.dylib"), &stub("/nix/libiconv.dylib", &[]));
    touch(&profile.join("libiconv.dylib"), &stub("/profile/libiconv.dylib", &[]));
    let missing = nix.join("missing");
    let answers =
        Answers::new().var("SDKROOT", sdk.display().to_string()).answer(DRIVER, jobs(&[&nix, &missing, &profile]));
    let cache = Cache::new();
    let output = executable(&cache, &answers, "aarch64-apple-darwin").unwrap();
    let search = [mine.clone()];
    let located = locate_plain(&cache, &answers, &output, &[dylib("iconv")], &search).unwrap();
    let shared = NativeKind::Shared { as_needed: false };
    assert_eq!(
        line(&located),
        [(nix.join("libiconv.dylib"), shared), (sdk.join("usr/lib/libSystem.tbd"), shared)],
        "the first directory the driver passes, after the link's own and before the SDK's"
    );

    let error = locate_plain(&cache, &answers, &output, &[dylib("nope")], &search).unwrap_err();
    let found = error.not_found().expect("a typed not-found");
    assert_eq!(found.library, "nope");
    assert_eq!(found.files, ["libnope.tbd", "libnope.dylib", "libnope.so", "libnope.a"]);
    assert_eq!(
        found.searched,
        [
            (mine.clone(), Searched::Link),
            (nix.clone(), Searched::CDriver),
            (profile.clone(), Searched::CDriver),
            (sdk.join("usr/lib"), Searched::Toolchain),
            (sdk.join("usr/local/lib"), Searched::Toolchain),
        ],
        "every directory searched, in order; one that is missing or relative is not passed"
    );
    assert!(error.what.contains(&format!("{} (the C driver's linker -L)", nix.display())), "{}", error.what);
    assert_eq!(error.no_room(), None);

    let error = executable(&Cache::new(), &Answers::new().var("SDKROOT", sdk.display().to_string()), "aarch64-apple-darwin")
        .unwrap_err();
    assert!(error.what.contains(DRIVER) && error.what.contains("neither CC_aarch64_apple_darwin nor CC"), "{}", error.what);
}

/// nix's libiconv re-exports libcharset by its absolute install name, which no `-L` names:
/// ld64 loads it with libiconv, so the line has it after libiconv, needed when imported from.
#[test]
fn a_macos_library_is_followed_by_the_libraries_it_reexports() {
    let sdk = temp("sdk-reexport");
    touch(&sdk.join("SDKSettings.json"), br#"{"Version":"15.0"}"#);
    touch(&sdk.join("usr/lib/libSystem.tbd"), &stub("/usr/lib/libSystem.B.dylib", &[]));
    touch(&sdk.join("usr/lib/libfake.1.tbd"), &stub("/usr/lib/libfake.1.dylib", &[]));
    let (iconv, charset) = (temp("nix-iconv"), temp("nix-charset"));
    let installed = charset.join("libcharset.1.dylib");
    let installed = installed.to_str().unwrap();
    let reexports = [installed, "/usr/lib/libfake.1.dylib"];
    touch(&iconv.join("libiconv.dylib"), &stub(iconv.join("libiconv.2.dylib").to_str().unwrap(), &reexports));
    touch(&charset.join("libcharset.1.dylib"), &stub(installed, &["@loader_path/deep/libdeep.dylib"]));
    touch(&charset.join("deep/libdeep.dylib"), &stub("/nowhere/libdeep.dylib", &[installed]));
    let answers = Answers::new().var("SDKROOT", sdk.display().to_string()).answer(DRIVER, jobs(&[&iconv]));
    let cache = Cache::new();
    let output = executable(&cache, &answers, "aarch64-apple-darwin").unwrap();
    let located = locate_plain(&cache, &answers, &output, &[dylib("iconv")], &[]).unwrap();
    let asked = NativeKind::Shared { as_needed: false };
    assert_eq!(
        line(&located),
        [
            (iconv.join("libiconv.dylib"), asked),
            (charset.join("libcharset.1.dylib"), SHARED),
            (charset.join("deep/libdeep.dylib"), SHARED),
            (sdk.join("usr/lib/libfake.1.tbd"), SHARED),
            (sdk.join("usr/lib/libSystem.tbd"), asked),
        ],
        "each re-export once, after its library, transitively; a `/usr/lib` one is the SDK's `.tbd`"
    );
    let search = std::slice::from_ref(&charset);
    let located = locate_plain(&cache, &answers, &output, &[dylib("iconv"), dylib("charset.1")], search).unwrap();
    assert_eq!(line(&located)[1], (charset.join("libcharset.1.dylib"), asked), "asked for too, it is loaded used or not");

    touch(&iconv.join("libiconv.dylib"), &stub("/nix/libiconv.2.dylib", &["/gone/libcharset.1.dylib"]));
    let error = locate_plain(&cache, &answers, &output, &[dylib("iconv")], &[]).unwrap_err();
    assert!(error.what.contains("re-exports /gone/libcharset.1.dylib (LC_REEXPORT_DYLIB)"), "{}", error.what);
    let found = error.not_found().expect("a typed not-found");
    assert_eq!(found.library, "/gone/libcharset.1.dylib");
    assert_eq!(found.files, ["libcharset.1.tbd", "libcharset.1.dylib"]);
    assert_eq!(
        found.searched,
        [(sdk.join("gone"), Searched::Toolchain), (PathBuf::from("/gone"), Searched::InstallName)],
        "under the SDK, then the install name's own directory"
    );

    touch(&iconv.join("libiconv.dylib"), &stub("/nix/libiconv.2.dylib", &["@rpath/libcharset.1.dylib"]));
    let error = locate_plain(&cache, &answers, &output, &[dylib("iconv")], &[]).unwrap_err();
    assert!(error.what.contains("@rpath/libcharset.1.dylib names the image's run"), "{}", error.what);
}

#[test]
fn a_windows_line_is_searched_in_lib_and_its_libraries_are_read_as_archives() {
    let (a, b) = (temp("lib-a"), temp("lib-b"));
    touch(&a.join("kernel32.lib"), ARCHIVE);
    touch(&b.join("foo.lib"), ARCHIVE);
    let answers = Answers::new().var("LIB", format!("{};{};", a.display(), b.display()));
    let cache = Cache::new();
    let output = executable(&cache, &answers, "x86_64-pc-windows-msvc").unwrap();
    let located = locate_plain(&cache, &answers, &output, &[dylib("kernel32"), static_lib("foo", true)], &[]).unwrap();
    assert_eq!(
        line(&located),
        [(a.join("kernel32.lib"), ARCHIVED), (b.join("foo.lib"), NativeKind::Archive { whole: true })]
    );
}

/// A COFF object whose `.drectve` is `directives`.
fn coff(directives: &str) -> Vec<u8> {
    let mut b = Builder::new(BinaryFormat::Coff, Architecture::X86_64, Endianness::Little);
    let drectve = b.add_section(Vec::new(), b".drectve".to_vec(), SectionKind::Linker);
    b.append_section_data(drectve, directives.as_bytes(), 1);
    b.write().unwrap()
}

/// An archive of `members`, without a symbol index: only its members' directives are read.
fn ar(members: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut out = ARCHIVE.to_vec();
    for (name, bytes) in members {
        let header = format!("{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n", format!("{name}/"), 0, 0, 0, 644, bytes.len());
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(bytes);
        if out.len() % 2 != 0 {
            out.push(b'\n');
        }
    }
    out
}

#[test]
fn a_windows_archive_is_followed_by_the_libraries_its_members_ask_for() {
    let (vc, kits) = (temp("vc"), temp("kits"));
    let crt = coff(" /DEFAULTLIB:vcruntime /DEFAULTLIB:\"ucrt.lib\" /DEFAULTLIB:libcmt /NODEFAULTLIB:LIBCMT /FAILIFMISMATCH:a=b");
    touch(&vc.join("msvcrt.lib"), &ar(&[("crt.obj", crt), ("exit.obj", coff("/DEFAULTLIB:MSVCRT /DEFAULTLIB:oldnames"))]));
    touch(&vc.join("vcruntime.lib"), &ar(&[("eh.obj", coff("/DEFAULTLIB:msvcrt.lib /DEFAULTLIB:kernel32"))]));
    touch(&vc.join("libcmt.lib"), ARCHIVE);
    touch(&vc.join("oldnames.lib"), ARCHIVE);
    touch(&kits.join("ucrt.lib"), ARCHIVE);
    touch(&kits.join("kernel32.lib"), ARCHIVE);
    let answers = Answers::new().var("LIB", format!("{};{}", vc.display(), kits.display()));
    let cache = Cache::new();
    let output = executable(&cache, &answers, "x86_64-pc-windows-msvc").unwrap();
    let libs = [dylib("msvcrt"), dylib("kernel32")];
    let expected = [
        (vc.join("msvcrt.lib"), ARCHIVED),
        (vc.join("vcruntime.lib"), ARCHIVED),
        (kits.join("kernel32.lib"), ARCHIVED),
        (kits.join("ucrt.lib"), ARCHIVED),
        (vc.join("oldnames.lib"), ARCHIVED),
    ];
    let located = locate_plain(&cache, &answers, &output, &libs, &[]).unwrap();
    assert_eq!(line(&located), expected, "each after the library that asked, each once, libcmt taken out by its asker");
    let read = cache.asked.lock().len();
    assert_eq!(read, 3, "msvcrt.lib, vcruntime.lib and the empty archives' one content");
    assert_eq!(line(&locate_plain(&cache, &answers, &output, &libs, &[]).unwrap()), expected);
    assert_eq!(cache.asked.lock().len(), read, "an archive's content is read for its libraries once");

    let mine = temp("mine-vc");
    touch(&mine.join("vcruntime.lib"), ARCHIVE);
    let located = locate_plain(&cache, &answers, &output, &libs, std::slice::from_ref(&mine)).unwrap();
    assert_eq!(line(&located)[1], (mine.join("vcruntime.lib"), ARCHIVED), "the link's directories before LIB's");

    touch(&vc.join("broken.lib"), &ar(&[("asks.obj", coff("/DEFAULTLIB:nowhere"))]));
    let error = locate_plain(&cache, &answers, &output, &[dylib("broken")], &[]).unwrap_err();
    for said in ["broken.lib(asks.obj", "nowhere.lib", "/DEFAULTLIB", vc.to_str().unwrap(), kits.to_str().unwrap()] {
        assert!(error.what.contains(said), "{said}: {}", error.what);
    }

    touch(&vc.join("quiet.lib"), &ar(&[("asks.obj", coff("/DEFAULTLIB:nowhere /NODEFAULTLIB"))]));
    let located = locate_plain(&cache, &answers, &output, &[dylib("quiet")], &[]).unwrap();
    assert_eq!(line(&located), [(vc.join("quiet.lib"), ARCHIVED)], "a bare /NODEFAULTLIB takes its member's all out");
}

#[test]
fn a_default_library_is_found_with_its_case_ignored() {
    let dir = temp("caseless");
    touch(&dir.join("ucrt.lib"), ARCHIVE);
    assert_eq!(caseless(&dir, "UCRT.LIB"), Some(dir.join("ucrt.lib")));
    assert_eq!(caseless(&dir, "ucrt.lib"), Some(dir.join("ucrt.lib")));
    assert_eq!(caseless(&dir, "vcruntime.lib"), None);
    assert_eq!(caseless(&dir.join("missing"), "ucrt.lib"), None);
}

#[test]
fn without_lib_the_msvc_and_sdk_directories_are_found_as_find_msvc_tools_finds_them() {
    let root = temp("msvc");
    let install = root.join("vs");
    touch(&install.join("VC/Auxiliary/Build/Microsoft.VCToolsVersion.default.txt"), b"14.40.33807\n");
    let tools = install.join("VC/Tools/MSVC/14.40.33807/lib/x64");
    std::fs::create_dir_all(&tools).unwrap();
    let kits = root.join("kits");
    touch(&kits.join("Lib/10.0.19041.0/ucrt/x64/ucrt.lib"), ARCHIVE);
    touch(&kits.join("Lib/10.0.22621.0/um/x64/kernel32.lib"), ARCHIVE);
    touch(&kits.join("Lib/10.0.22000.0/um/x64/kernel32.lib"), ARCHIVE);
    let vswhere = root.join("Microsoft Visual Studio").join("Installer").join("vswhere.exe");
    let answers = Answers::new()
        .var("ProgramFiles(x86)", root.display().to_string())
        .answer(
            format!(
                "{} -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath",
                vswhere.display()
            ),
            Ok(format!("{}\r\n", install.display())),
        )
        .answer(
            r"reg query HKLM\SOFTWARE\Microsoft\Windows Kits\Installed Roots /v KitsRoot10 /reg:32",
            Ok(format!(
                "\r\nHKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Windows Kits\\Installed Roots\r\n    KitsRoot10    REG_SZ    {}\r\n\r\n",
                kits.display()
            )),
        );
    let cache = Cache::new();
    let output = executable(&cache, &answers, "x86_64-pc-windows-msvc").unwrap();
    let located = locate_plain(&cache, &answers, &output, &[dylib("kernel32")], &[]).unwrap();
    assert_eq!(line(&located), [(kits.join("Lib/10.0.22621.0/um/x64/kernel32.lib"), ARCHIVED)]);

    let error = locate_plain(&Cache::new(), &Answers::new(), &output, &[], &[]).unwrap_err();
    assert!(error.what.contains("LIB is not set") && error.what.contains("ProgramFiles(x86)"), "{}", error.what);
}

#[test]
fn a_wasi_line_is_wasi_libcs_from_blazes_toolchain() {
    let sysroot = temp("sysroot");
    let dir = sysroot.join("lib/rustlib/wasm32-wasip1/lib/self-contained");
    touch(&dir.join("crt1-command.o"), b"\0asm\x01\0\0\0");
    touch(&dir.join("libc.a"), ARCHIVE);
    let answers = Answers::new();
    let cache = Cache::new();
    let output = executable(&cache, &answers, "wasm32-wasip1").unwrap();
    let libc = static_lib("c", false);
    let located = locate_with(&cache, &answers, &output, std::slice::from_ref(&libc), &[], &sysroot).unwrap();
    assert_eq!(line(&located), [(dir.join("crt1-command.o"), NativeKind::Object), (dir.join("libc.a"), ARCHIVED)]);
    let output = shared(&cache, &answers, "wasm32-wasip1").unwrap();
    let located = locate_with(&cache, &answers, &output, &[], &[], &sysroot).unwrap();
    assert_eq!(line(&located), [(dir.join("libc.a"), ARCHIVED)], "a cdylib has no command start");

    let empty = temp("no-wasi-libc");
    let output = executable(&cache, &answers, "wasm32-wasip1").unwrap();
    let error = locate_with(&cache, &answers, &output, &[], &[], &empty).unwrap_err();
    assert!(error.what.contains("no wasi-libc") && error.what.contains(empty.to_str().unwrap()), "{}", error.what);

    let output = shared(&cache, &answers, "wasm32-unknown-unknown").unwrap();
    assert!(locate_plain(&cache, &answers, &output, &[], &[]).unwrap().is_empty());
}

#[test]
fn versions_and_the_sdks_settings_are_read_as_rustc_reads_them() {
    assert_eq!(parse_version("11"), Some(0x000b_0000));
    assert_eq!(parse_version("10.15.7"), Some(0x000a_0f07));
    for bad in ["", "11.", "11.0.0.0", "x", "11.256", "+11", "11 .0"] {
        assert_eq!(parse_version(bad), None, "{bad}");
    }
    let json = r#"{ "Nested": {"Version": "1"}, "List": ["Version", {"Version": "2"}], "Name": "Version", "Version" : "15.2" }"#;
    assert_eq!(top_level_string(json, "Version"), Some("15.2"));
    assert_eq!(top_level_string(r#"{"Version": 15}"#, "Version"), None);
    assert_eq!(top_level_string(r#"{"Other": "a\"Version\": \"9\""}"#, "Version"), None);
}

/// An AArch64 Mach-O object defining `_f` whose `LC_LINKER_OPTION`s are `options`.
fn autolinking(options: &[&[&'static str]]) -> Vec<u8> {
    crate::macho::linking_object(object::macho::CPU_TYPE_ARM64, &["_f"], options)
}

#[test]
fn a_macos_line_is_followed_by_what_its_objects_and_members_ask_for() {
    let (sdk, mine) = (temp("sdk-autolink"), temp("mine-autolink"));
    touch(&sdk.join("SDKSettings.json"), br#"{"Version":"15.0"}"#);
    let tbd = &stub("/usr/lib/libstub.dylib", &[]);
    touch(&sdk.join("usr/lib/libSystem.tbd"), tbd);
    touch(&sdk.join("usr/lib/libz.tbd"), tbd);
    let frameworks = sdk.join("System/Library/Frameworks");
    touch(&frameworks.join("Security.framework/Security.tbd"), tbd);
    touch(&mine.join("glue.o"), &autolinking(&[&["-lz"]]));
    let sec = ar(&[("a.o", autolinking(&[&["-framework", "Security"]])), ("b.o", autolinking(&[&["-lhelper"]]))]);
    touch(&mine.join("libsec.a"), &sec);
    touch(&mine.join("libhelper.a"), &ar(&[("h.o", autolinking(&[&["-lz"], &["-framework", "Security"]]))]));
    let answers = Answers::new().var("SDKROOT", sdk.display().to_string()).answer(DRIVER, jobs(&[]));
    let cache = Cache::new();
    let output = executable(&cache, &answers, "aarch64-apple-darwin").unwrap();
    let search = std::slice::from_ref(&mine);
    let libs = [verbatim("glue.o"), static_lib("sec", false)];
    let shared = NativeKind::Shared { as_needed: false };
    let expected = [
        (mine.join("glue.o"), NativeKind::Object),
        (sdk.join("usr/lib/libz.tbd"), shared),
        (mine.join("libsec.a"), ARCHIVED),
        (frameworks.join("Security.framework/Security.tbd"), shared),
        (mine.join("libhelper.a"), ARCHIVED),
        (sdk.join("usr/lib/libSystem.tbd"), shared),
    ];
    let located = locate_plain(&cache, &answers, &output, &libs, search).unwrap();
    assert_eq!(line(&located), expected, "an object's -lz, a member's -framework, transitively, each once after its asker");
    let read = cache.asked.lock().len();
    assert_eq!(read, 4, "glue.o, libsec.a and libhelper.a, and the stubs' re-exports");
    assert_eq!(line(&locate_plain(&cache, &answers, &output, &libs, search).unwrap()), expected);
    assert_eq!(cache.asked.lock().len(), read, "a file's content is read for its options once");

    touch(&mine.join("lost.o"), &autolinking(&[&["-lnowhere"]]));
    let error = locate_plain(&cache, &answers, &output, &[verbatim("lost.o")], search).unwrap_err();
    for said in ["lost.o asks for -lnowhere (LC_LINKER_OPTION)", "libnowhere.tbd", mine.to_str().unwrap(), sdk.join("usr/lib").to_str().unwrap()] {
        assert!(error.what.contains(said), "{said}: {}", error.what);
    }
    touch(&mine.join("libbroken.a"), &ar(&[("m.o", autolinking(&[&["-framework", "Missing"]]))]));
    let error = locate_plain(&cache, &answers, &output, &[static_lib("broken", false)], search).unwrap_err();
    for said in ["libbroken.a(m.o) asks for -framework Missing", "Missing.framework/Missing.tbd", frameworks.to_str().unwrap()] {
        assert!(error.what.contains(said), "{said}: {}", error.what);
    }

    touch(&mine.join("libodd.a"), &ar(&[("n.o", autolinking(&[&["-needed_framework", "Security"]]))]));
    let error = locate_plain(&cache, &answers, &output, &[static_lib("odd", false)], search).unwrap_err();
    assert!(error.what.contains("libodd.a(n.o)") && error.what.contains("`-needed_framework Security`"), "{}", error.what);
    touch(&mine.join("weak.o"), &autolinking(&[&["-weak_framework", "Security"]]));
    let error = locate_plain(&cache, &answers, &output, &[verbatim("weak.o")], search).unwrap_err();
    assert!(error.what.contains("weak.o") && error.what.contains("no weak library"), "{}", error.what);
}

fn sanitized(mut output: Output, sanitizers: Sanitizers) -> Output {
    output.sanitize.sanitizers = sanitizers;
    output
}

const SANITIZERS: [(Sanitizers, &str, &str); 3] = [
    (Sanitizers::ADDRESS, "asan", "AddressSanitizer"),
    (Sanitizers::THREAD, "tsan", "ThreadSanitizer"),
    (Sanitizers::MEMORY, "msan", "MemorySanitizer"),
];

#[test]
fn a_linux_sanitizers_runtime_is_its_whole_archive_after_the_start_objects() {
    let glibc = glibc(Target::LinuxX86_64);
    let cache = Cache::new();
    let sysroot = temp("sanitize-linux");
    let dir = sysroot.join("lib/rustlib/x86_64-unknown-linux-gnu/lib");
    for (_, name, _) in SANITIZERS {
        touch(&dir.join(format!("librustc-nightly_rt.{name}.a")), ARCHIVE);
    }
    let plain = executable(&cache, &glibc.answers, "x86_64-unknown-linux-gnu").unwrap();
    let without = line(&locate_with(&cache, &glibc.answers, &plain, &[], &[], &sysroot).unwrap());
    for (sanitizer, name, _) in SANITIZERS {
        let output = sanitized(plain.clone(), sanitizer);
        let located = line(&locate_with(&cache, &glibc.answers, &output, &[], &[], &sysroot).unwrap());
        let runtime = (dir.join(format!("librustc-nightly_rt.{name}.a")), NativeKind::Archive { whole: true });
        let mut expected = without.clone();
        expected.insert(3, runtime);
        assert_eq!(located, expected, "{name}: after crtbeginS.o, before the libraries, whole");
        assert!(rpaths(Target::LinuxX86_64, &output, &sysroot).unwrap().is_empty(), "{name}: an archive needs no rpath");
    }

    let output = sanitized(shared(&cache, &glibc.answers, "x86_64-unknown-linux-gnu").unwrap(), Sanitizers::ADDRESS);
    let located = locate_with(&cache, &glibc.answers, &output, &[], &[], &sysroot).unwrap();
    assert!(located.iter().all(|it| !it.path.starts_with(&dir)), "an ELF shared library leaves the runtime to its executable");

    let empty = temp("sanitize-linux-empty");
    for (sanitizer, name, what) in SANITIZERS {
        let output = sanitized(plain.clone(), sanitizer);
        let error = locate_with(&cache, &glibc.answers, &output, &[], &[], &empty).unwrap_err();
        let path = empty.join(format!("lib/rustlib/x86_64-unknown-linux-gnu/lib/librustc-nightly_rt.{name}.a"));
        assert!(error.what.contains(what) && error.what.contains(path.to_str().unwrap()), "{}", error.what);
    }
    let both = sanitized(plain.clone(), Sanitizers::ADDRESS.union(Sanitizers::MEMORY));
    let error = locate_with(&cache, &glibc.answers, &both, &[], &[], &sysroot).unwrap_err();
    assert!(error.what.contains("AddressSanitizer and MemorySanitizer"), "{}", error.what);
}

#[test]
fn a_macos_sanitizers_runtime_is_its_dylib_found_by_an_absolute_rpath() {
    let sdk = temp("sanitize-sdk");
    touch(&sdk.join("SDKSettings.json"), br#"{"Version":"15.0"}"#);
    touch(&sdk.join("usr/lib/libSystem.tbd"), &stub("/usr/lib/libSystem.B.dylib", &[]));
    let answers = Answers::new().var("SDKROOT", sdk.display().to_string()).answer(DRIVER, jobs(&[]));
    let cache = Cache::new();
    let sysroot = temp("sanitize-macos");
    let dir = sysroot.join("lib/rustlib/aarch64-apple-darwin/lib");
    for name in ["asan", "tsan"] {
        touch(&dir.join(format!("librustc-nightly_rt.{name}.dylib")), &stub("@rpath/librt.dylib", &[]));
    }
    let plain = executable(&cache, &answers, "aarch64-apple-darwin").unwrap();
    assert!(rpaths(Target::MacOs, &plain, &sysroot).unwrap().is_empty(), "no sanitizer, no rpath");
    let shared_kind = NativeKind::Shared { as_needed: false };
    for (sanitizer, name, _) in &SANITIZERS[..2] {
        let output = sanitized(plain.clone(), *sanitizer);
        let located = line(&locate_with(&cache, &answers, &output, &[], &[], &sysroot).unwrap());
        assert_eq!(
            located,
            [(dir.join(format!("librustc-nightly_rt.{name}.dylib")), shared_kind), (sdk.join("usr/lib/libSystem.tbd"), shared_kind)],
            "{name}"
        );
        assert_eq!(rpaths(Target::MacOs, &output, &sysroot).unwrap(), [dir.to_str().unwrap()], "{name}");
    }
    let output = sanitized(plain.clone(), Sanitizers::MEMORY);
    let error = locate_with(&cache, &answers, &output, &[], &[], &sysroot).unwrap_err();
    assert!(error.what.contains("MemorySanitizer") && error.what.contains("only on Linux"), "{}", error.what);

    let error =
        locate_with(&cache, &answers, &sanitized(plain, Sanitizers::ADDRESS), &[], &[], &temp("sanitize-macos-empty")).unwrap_err();
    assert!(error.what.contains("AddressSanitizer's runtime") && error.what.contains("librustc-nightly_rt.asan.dylib"), "{}", error.what);
    let relative = sanitized(executable(&cache, &answers, "aarch64-apple-darwin").unwrap(), Sanitizers::THREAD);
    let error = rpaths(Target::MacOs, &relative, Path::new("toolchain")).unwrap_err();
    assert!(error.what.contains("not absolute"), "{}", error.what);
}

#[test]
fn a_sanitizer_on_windows_or_wasm_is_an_error() {
    let sysroot = temp("sanitize-other");
    let cache = Cache::new();
    let answers = Answers::new();
    for (sanitizer, _, what) in SANITIZERS {
        let windows = sanitized(executable(&cache, &answers, "x86_64-pc-windows-msvc").unwrap(), sanitizer);
        let error = locate_with(&cache, &answers, &windows, &[], &[], &sysroot).unwrap_err();
        assert!(error.what.contains(what) && error.what.contains("/INFERASANLIBS"), "{}", error.what);
        let wasm = sanitized(executable(&cache, &answers, "wasm32-wasip1").unwrap(), sanitizer);
        let error = locate_with(&cache, &answers, &wasm, &[], &[], &sysroot).unwrap_err();
        assert!(error.what.contains(what) && error.what.contains("wasm"), "{}", error.what);
    }
}
