//! The acceptance ladder's toolchain rungs (docs/v2/plan/design/c.md "Proof on real libraries"):
//! the real blaze-cfront helper, the pinned clang and the target's headers, run over the real
//! corpus, every file served from the roots by [`driver::run_native`] and nothing else. Each rung
//! is a process test (`::kind_process`): its request, what it asserts, and the counts it prints.
//!
//! What a rung needs is one directory, named when the test binary is built
//! (`BLAZE_CC_LADDER=<dir> cargo test -p blaze-cc -- ::kind_process`), so the test reads nothing
//! ambient at run time:
//!
//! ```text
//! <dir>/blaze-cfront           the helper (llvm.py `"cfront": true`, or toolchain_cut's shelf copy)
//! <dir>/shelf/clang/           the pinned clang prefix (bin/clang, lib/clang/23/include)
//! <dir>/shelf/sdk/             the macOS SDK (c.md decision 1) or the pinned Linux sysroot (3)
//! <dir>/ref-c-corpus/          -> /Volumes/Projects/ref-c-corpus (numpy, CPython)
//! <dir>/registry/              -> ~/.cargo/registry/src/index.crates.io-<hash>
//! ```
//!
//! A rung whose helper or source is not there passes saying so by name ("skipped: ..."): the
//! printed line is the evidence of what ran (rule 61), and a skip is never counted as a pass of
//! the rung in c.md's table.

#![cfg(unix)]

use core::time::Duration;
use std::{
    format, fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    println,
    string::String as StdString,
    vec::Vec as StdVec,
};

use blaze_c::{CAbi, DeclsBuilder, DeclsOpts, Endian, Root as Place, Scalar, front::abi_row};
use blaze_own::{AVec, Alloc, planted::NoPlant};
use blaze_roots::{Digest, Glob, Mount, Mounts, RelPath, Root, RootId, RootKind, Roots, Seen};

use crate::{
    decls::{Census, DeclsLimits, DeclsReader, DeclsReaderOpts},
    driver::{self, Looked, View},
    flags::{self, Debug as DebugInfo, Define, Lang, Opt, Source, Target, Toolchain, Unit},
    job::{Finished, Job, JobOpts, Limits},
    wire::{Confinement, LookOp, Mode},
};

/// The ladder's directory, as the test binary was built.
const LADDER: Option<&str> = option_env!("BLAZE_CC_LADDER");

const SHELF: RootId = RootId(0);
const PACKAGE: RootId = RootId(1);

const MOUNTS: [Mount<'static>; 2] = [Mount { spelling: b"/shelf", root: SHELF }, Mount { spelling: b"/package", root: PACKAGE }];

const TOOLS: Toolchain<'static> = Toolchain { clang: "/shelf/clang", sysroot: "/shelf/sdk", package: "/package" };

const PLACES: [(RootId, Place); 2] = [(SHELF, Place::Shelf), (PACKAGE, Place::Package)];

/// How long one helper run may take (the sqlite amalgamation at `-O0` is the longest: an
/// estimate of seconds; ten minutes is the hang bound, rule 69).
const RUN_BUDGET: Duration = Duration::from_secs(600);

fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// One rung: an import of `headers`, and for some a unit compiled.
struct Rung {
    name: &'static str,
    /// The package's directory under the ladder's.
    package: &'static str,
    /// What the package root admits.
    globs: &'static [&'static str],
    include: &'static [&'static str],
    headers: &'static [&'static str],
    defines: &'static [Define<'static>],
    /// Lines the declaration document must hold, each the start of a line (`fn NAME `).
    lines: &'static [&'static str],
    /// The least functions and constants the model must take.
    least_functions: u32,
    least_constants: u32,
    /// A source file compiled to an object, the code record's input.
    compile: Option<&'static str>,
}

/// The target's C facts, for the reader's integer checks.
fn host_abi() -> CAbi {
    const fn sc(size: u8, align: u8) -> Scalar {
        Scalar { size, align }
    }
    let macos = cfg!(target_os = "macos");
    let x86 = cfg!(target_arch = "x86_64");
    CAbi {
        endian: Endian::Little,
        // AArch64 Linux's `char` is unsigned; Apple's and x86-64's signed.
        char_signed: macos || x86,
        pointer: sc(8, 8),
        bool_: sc(1, 1),
        short: sc(2, 2),
        int: sc(4, 4),
        long: sc(8, 8),
        long_long: sc(8, 8),
        int128: Some(sc(16, 16)),
        float: sc(4, 4),
        double: sc(8, 8),
        long_double: if macos { sc(8, 8) } else { sc(16, 16) },
        max_align: if macos { 8 } else { 16 },
    }
}

fn host_target() -> (Target, Confinement) {
    if cfg!(target_os = "macos") {
        (Target::MacosArm64 { min_major: 13, min_minor: 0 }, Confinement::Seatbelt)
    } else if cfg!(target_arch = "x86_64") {
        (Target::LinuxX64, Confinement::Seccomp)
    } else {
        (Target::LinuxArm64, Confinement::Seccomp)
    }
}

/// The roots read from the host's disk: each lookup the machine resolved and admitted.
struct HostView<'g> {
    shelf: PathBuf,
    package: PathBuf,
    roots: &'g Roots<'g>,
}

impl View for HostView<'_> {
    type Error = std::io::Error;

    fn look(&self, op: LookOp, root: RootId, path: RelPath<'_>, into: &mut AVec<'_, u8>) -> Result<Looked, std::io::Error> {
        let base = if root == SHELF { &self.shelf } else { &self.package };
        let host = if path.is_root() { base.clone() } else { base.join(path.as_str()) };
        let meta = match fs::metadata(&host) {
            Ok(meta) => meta,
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => return Ok(Looked::Missing),
            Err(e) => return Err(e),
        };
        if meta.is_dir() {
            if op != LookOp::List {
                return Ok(Looked::Dir);
            }
            let Some(r) = self.roots.get(root) else { return Ok(Looked::Missing) };
            let mut names: StdVec<StdString> = StdVec::new();
            for entry in fs::read_dir(&host)? {
                let entry = entry?;
                let Ok(name) = entry.file_name().into_string() else { continue };
                let child = if path.is_root() { name.clone() } else { format!("{}/{name}", path.as_str()) };
                let Ok(rel) = RelPath::new(&child, 4096) else { continue };
                if entry.file_type()?.is_dir() {
                    if r.admits_dir(rel) {
                        names.push(format!("{name}/"));
                    }
                } else if r.admits_file(rel) {
                    names.push(name);
                }
            }
            names.sort();
            for name in &names {
                into.extend_from_slice(name.as_bytes());
                into.push(0);
            }
            return Ok(Looked::Listing);
        }
        // A stat is answered with the digest too: the read list holds what the file is.
        let bytes = fs::read(&host)?;
        let digest = blaze_blake3::hash(&bytes);
        if op == LookOp::Read {
            into.extend_from_slice(&bytes);
        }
        Ok(Looked::File { digest, size: bytes.len() as u64 })
    }
}

fn rel(text: &str) -> RelPath<'_> {
    match RelPath::new(text, 4096) {
        Ok(path) => path,
        Err(why) => panic!("ladder: {text:?} is not a relative path: {why:?}"),
    }
}

/// One helper run of `unit` in `mode`, its outputs and checked read list.
fn run<'a>(base: &Path, rung: &Rung, roots: &Roots<'_>, mounts: &Mounts<'_>, unit: &Unit<'_>, mode: Mode, alloc: Alloc<'a>) -> Finished<'a> {
    let (_, confinement) = host_target();
    let args = match flags::args(unit, TOOLS, mode, alloc) {
        Ok(args) => args,
        Err(fault) => panic!("ladder: {}: the unit's arguments: {fault:?}", rung.name),
    };
    let io = match bstd::io::Io::native(bstd::io::NativeWorld {
        root: base,
        threads: 1,
        env: bstd::env::Vars::given(StdVec::new()),
        args: bstd::args::ArgList::given(StdVec::new()),
        host_name: std::sync::Arc::from(""),
        seed: Some(0),
        above: 0,
        limits: bstd::io::Limits::USUAL,
    }) {
        Ok(io) => io,
        Err(_) => panic!("ladder: {}: no native world over {}", rung.name, base.display()),
    };
    let Ok(spawner) = io.need_host().and_then(|host| host.spawner()) else {
        panic!("ladder: {}: this world starts no process", rung.name)
    };
    let Ok(clock) = io.need_clock() else { panic!("ladder: {}: this world has no clock", rung.name) };
    let Ok(cx) = io.cx(RUN_BUDGET.saturating_add(Duration::from_secs(60)), bstd::mem::TASK_FRAME) else {
        panic!("ladder: {}: its world stopped", rung.name)
    };
    let deadline = driver::nanos(clock.now().since_start().saturating_add(RUN_BUDGET));
    let mut job = Job::new(JobOpts {
        alloc,
        roots,
        mounts,
        mode,
        args,
        limits: Limits::STANDARD,
        deadline,
        confinement,
        plant: NoPlant,
    });
    let view = HostView { shelf: base.join("shelf"), package: base.join(rung.package), roots };
    let helper = base.join("blaze-cfront");
    let Some(helper) = helper.to_str() else { panic!("ladder: {}: the helper's path is not UTF-8", rung.name) };
    if let Err(e) = driver::run_native(&cx, &spawner, helper, &view, &mut job, alloc) {
        panic!("ladder: {}: the driver stopped: {e:?}", rung.name);
    }
    let diagnostics = format!("{:?}", job.diagnostics().first_error());
    let finished = match job.finish() {
        Ok(finished) => finished,
        Err(e) => panic!("ladder: {}: the job failed: {e:?}; first error {diagnostics}", rung.name),
    };
    if finished.errors {
        panic!("ladder: {}: the source has errors; first {diagnostics}", rung.name);
    }
    finished
}

fn print_reads(rung: &Rung, what: &str, finished: &Finished<'_>) {
    let (mut files, mut absent, mut dirs, mut listings) = (0u32, 0u32, 0u32, 0u32);
    for entry in finished.reads.entries() {
        match entry.seen {
            Seen::File { .. } => files += 1,
            Seen::Absent => absent += 1,
            Seen::Dir => dirs += 1,
            Seen::Listing { .. } => listings += 1,
        }
    }
    let shelf = finished.reads.entries().filter(|e| e.root == SHELF).count();
    println!(
        "ladder: {} {what}: reads {files} files, {absent} absent, {dirs} directories, {listings} listings ({shelf} on the shelf); {} diagnostics",
        rung.name,
        finished.diagnostics.len()
    );
}

fn print_census(rung: &Rung, c: &Census) {
    let causes: StdVec<StdString> =
        c.unexpressed.iter().enumerate().filter(|(_, n)| **n > 0).map(|(cause, n)| format!("{cause}:{n}")).collect();
    println!(
        "ladder: {} decls: {} headers, {} types, {} records ({} laid out), {} enums, {} typedefs, {} functions, {} variables, {} constants, {} left out by cause [{}]",
        rung.name,
        c.headers,
        c.types,
        c.records,
        c.layouts,
        c.enums,
        c.typedefs,
        c.functions,
        c.variables,
        c.constants,
        c.left_out(),
        causes.join(" ")
    );
}

/// The rung run: skipped by name when its helper or source is absent.
fn climb(rung: &Rung) {
    let Some(base) = LADDER else {
        println!("ladder: {}: skipped: BLAZE_CC_LADDER was not set when this test was built", rung.name);
        return;
    };
    let base = Path::new(base);
    if !base.join("blaze-cfront").is_file() {
        println!("ladder: {}: skipped: no helper at {}", rung.name, base.join("blaze-cfront").display());
        return;
    }
    if !base.join(rung.package).is_dir() {
        println!("ladder: {}: skipped: source not on disk at {}", rung.name, base.join(rung.package).display());
        return;
    }
    let globs: StdVec<Glob<'static>> = rung
        .globs
        .iter()
        .map(|&g| match Glob::new(g) {
            Ok(glob) => glob,
            Err(why) => panic!("ladder: {}: glob {g:?}: {why:?}", rung.name),
        })
        .collect();
    let table = [
        // The ladder checks no key, so the shelf's tree digest is not computed here.
        Root { name: "shelf", kind: RootKind::Tree { tree: Digest([0; 32]) } },
        Root { name: "package", kind: RootKind::Package { globs: &globs } },
    ];
    let Ok(roots) = Roots::new(&table) else { panic!("ladder: {}: its roots", rung.name) };
    let Ok(mounts) = Mounts::new(&MOUNTS, None, &roots) else { panic!("ladder: {}: its mounts", rung.name) };
    let (target, _) = host_target();
    let include: StdVec<RelPath<'_>> = rung.include.iter().map(|d| rel(d)).collect();
    let headers: StdVec<RelPath<'_>> = rung.headers.iter().map(|h| rel(h)).collect();
    let import = Unit {
        lang: Lang::C,
        std: "gnu17",
        target,
        source: Source::Headers(&headers),
        include: &include,
        defines: rung.defines,
        opt: Opt::Quick,
        debug: DebugInfo::None,
    };
    let alloc = heap();

    // The import: the declaration document, read into blaze-c's model.
    let finished = run(base, rung, &roots, &mounts, &import, Mode::Decls, alloc);
    print_reads(rung, "decls", &finished);
    let Ok(text) = core::str::from_utf8(&finished.decls) else { panic!("ladder: {}: the document is not UTF-8", rung.name) };
    for line in rung.lines {
        assert!(
            text.starts_with(line) || text.contains(&format!("\n{line}")),
            "ladder: {}: the document has no line starting {line:?}",
            rung.name
        );
    }
    let abi = host_abi();
    let mut reader = DeclsReader::new(DeclsReaderOpts {
        into: DeclsBuilder::new(DeclsOpts { abi: abi_row(&abi), alloc }),
        abi,
        mounts: &mounts,
        places: &PLACES,
        limits: DeclsLimits::STANDARD,
        alloc,
        plant: NoPlant,
    });
    if let Err(e) = reader.feed(&finished.decls) {
        panic!("ladder: {}: the document is refused: {e:?}", rung.name);
    }
    let census = match reader.finish() {
        Ok((_, census)) => census,
        Err(e) => panic!("ladder: {}: the document is refused: {e:?}", rung.name),
    };
    print_census(rung, &census);
    assert!(census.functions >= rung.least_functions, "ladder: {}: {} functions, fewer than {}", rung.name, census.functions, rung.least_functions);
    assert!(census.constants >= rung.least_constants, "ladder: {}: {} constants, fewer than {}", rung.name, census.constants, rung.least_constants);
    // Every header the helper read is on the read list: none read from outside the roots.
    assert!(finished.reads.entries().any(|e| e.root == PACKAGE), "ladder: {}: the package was never read", rung.name);

    // The unit: an object, one section a function (blaze-cunit's code records).
    if let Some(source) = rung.compile {
        let unit = Unit { source: Source::File(rel(source)), opt: Opt::Fast, ..import };
        let finished = run(base, rung, &roots, &mounts, &unit, Mode::Compile, alloc);
        print_reads(rung, "compile", &finished);
        let object = finished.object.as_slice();
        println!("ladder: {} compile: {source}: an object of {} bytes", rung.name, object.len());
        if cfg!(target_os = "macos") {
            // A 64-bit Mach-O (MH_MAGIC_64, little-endian) whose flags hold
            // MH_SUBSECTIONS_VIA_SYMBOLS (0x2000): clang's Darwin default, the atom per symbol.
            assert_eq!(object.get(..4), Some(&[0xcf, 0xfa, 0xed, 0xfe][..]), "ladder: {}: not a 64-bit Mach-O", rung.name);
            let flags = object.get(24..28).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
            assert!(flags.is_some_and(|f| f & 0x2000 != 0), "ladder: {}: no MH_SUBSECTIONS_VIA_SYMBOLS", rung.name);
        } else {
            // An ELF object with a `.text.<function>` section (`-ffunction-sections`).
            assert_eq!(object.get(..4), Some(&b"\x7fELF"[..]), "ladder: {}: not an ELF object", rung.name);
            assert!(object.windows(6).any(|w| w == b".text."), "ladder: {}: no section per function", rung.name);
        }
    }
}

const NO_DEFINES: &[Define<'static>] = &[];

const ZSTD: Rung = Rung {
    name: "zstd",
    package: "registry/zstd-sys-2.1.3+zstd.1.5.7",
    globs: &["zstd/lib/**"],
    include: &["zstd/lib"],
    headers: &["zstd/lib/zstd.h"],
    defines: NO_DEFINES,
    lines: &["fn ZSTD_compress ", "fn ZSTD_decompress ", "macro int ZSTD_VERSION_MAJOR "],
    least_functions: 100,
    least_constants: 10,
    compile: Some("zstd/lib/common/entropy_common.c"),
};

const ZLIB: Rung = Rung {
    name: "zlib",
    package: "registry/libz-sys-1.1.31",
    globs: &["src/zlib/**"],
    include: &["src/zlib"],
    headers: &["src/zlib/zlib.h"],
    defines: NO_DEFINES,
    lines: &["fn deflate ", "fn inflate ", "record 0 z_stream_s ", "macro str ZLIB_VERSION "],
    least_functions: 80,
    least_constants: 30,
    compile: Some("src/zlib/adler32.c"),
};

const LZ4: Rung = Rung {
    name: "lz4",
    package: "registry/lz4-sys-1.11.1+lz4-1.10.0",
    globs: &["liblz4/lib/**"],
    include: &["liblz4/lib"],
    headers: &["liblz4/lib/lz4.h"],
    defines: NO_DEFINES,
    lines: &["fn LZ4_compress_default ", "macro int LZ4_VERSION_MAJOR "],
    least_functions: 20,
    least_constants: 3,
    compile: None,
};

const SQLITE: Rung = Rung {
    name: "sqlite",
    package: "registry/libsqlite3-sys-0.39.1",
    globs: &["sqlite3/**"],
    include: &["sqlite3"],
    headers: &["sqlite3/sqlite3.h"],
    defines: NO_DEFINES,
    lines: &["fn sqlite3_open ", "macro int SQLITE_OK ", "macro str SQLITE_VERSION "],
    least_functions: 250,
    least_constants: 300,
    // The amalgamation: one unit of about nine megabytes, the scale case of the compile.
    compile: Some("sqlite3/sqlite3.c"),
};

const ROCKSDB: Rung = Rung {
    name: "rocksdb",
    package: "registry/librocksdb-sys-0.17.3+10.4.2",
    globs: &["rocksdb/include/**"],
    include: &["rocksdb/include"],
    headers: &["rocksdb/include/rocksdb/c.h"],
    defines: NO_DEFINES,
    lines: &["fn rocksdb_open ", "record 0 rocksdb_t "],
    least_functions: 500,
    least_constants: 0,
    compile: None,
};

const BLST: Rung = Rung {
    name: "blst",
    package: "registry/blst-0.3.17",
    globs: &["blst/bindings/**"],
    include: &["blst/bindings"],
    headers: &["blst/bindings/blst.h"],
    defines: NO_DEFINES,
    lines: &["fn blst_keygen "],
    least_functions: 100,
    least_constants: 0,
    compile: None,
};

const AWS_LC: Rung = Rung {
    name: "aws-lc",
    package: "registry/aws-lc-sys-0.47.0",
    globs: &["aws-lc/include/**"],
    include: &["aws-lc/include"],
    headers: &["aws-lc/include/openssl/sha.h"],
    defines: NO_DEFINES,
    lines: &["fn SHA256 ", "macro int SHA256_DIGEST_LENGTH "],
    least_functions: 20,
    least_constants: 5,
    compile: None,
};

const C_KZG: Rung = Rung {
    name: "c-kzg",
    package: "registry/c-kzg-2.1.8",
    globs: &["src/**", "blst/bindings/**"],
    include: &["src", "blst/bindings"],
    headers: &["src/ckzg.h"],
    defines: NO_DEFINES,
    lines: &["fn blob_to_kzg_commitment "],
    least_functions: 10,
    least_constants: 0,
    compile: None,
};

/// CPython alone: `Python.h` over the 3.13 headers (with their pyconfig.h).
const CPYTHON: Rung = Rung {
    name: "cpython",
    package: "ref-c-corpus",
    globs: &["cpython-3.13-include/**"],
    include: &["cpython-3.13-include"],
    headers: &["cpython-3.13-include/Python.h"],
    defines: NO_DEFINES,
    // `Py_INCREF` is a static inline (object.h:794: a `fn` row of internal linkage with its
    // body) wrapped by a function-like macro of its own name (`Py_INCREF(_PyObject_CAST(op))`,
    // under `Py_LIMITED_API` < 3.11): the macro is a left-out row, the function is not.
    // `Py_None` is `(&_Py_NoneStruct)`: an address-of constant.
    lines: &["fn PyObject_GetAttr ", "fn Py_Initialize ", "record 0 _object ", "fn Py_INCREF ", "unexpressed Py_INCREF ", "macro addr Py_None "],
    least_functions: 1000,
    least_constants: 500,
    compile: None,
};

/// numpy's C API over CPython's: `arrayobject.h`, which reaches `Python.h`.
const NUMPY: Rung = Rung {
    name: "numpy",
    package: "ref-c-corpus",
    globs: &["numpy-wheel/numpy/_core/include/**", "cpython-3.13-include/**"],
    include: &["cpython-3.13-include", "numpy-wheel/numpy/_core/include"],
    headers: &["numpy-wheel/numpy/_core/include/numpy/arrayobject.h"],
    defines: NO_DEFINES,
    // `PyArray_NDIM` and `PyArray_DATA` are static inlines (ndarraytypes.h:1594, 1600);
    // `PyArray_API` is a `static void **` in the header (__multiarray_api.h:644), and its 303
    // slots are object-like macros over it (`#define PyArray_Type (*(PyTypeObject *)PyArray_API[2])`).
    lines: &["fn PyArray_NDIM ", "fn PyArray_DATA ", "var PyArray_API ", "macro slot PyArray_Type "],
    least_functions: 1000,
    least_constants: 500,
    compile: None,
};

blaze_sim::kind_test! {
    /// zstd.h read and one of its units compiled.
    process fn rung_zstd() {
        climb(&ZSTD);
    }
}

blaze_sim::kind_test! {
    process fn rung_zlib() {
        climb(&ZLIB);
    }
}

blaze_sim::kind_test! {
    process fn rung_lz4() {
        climb(&LZ4);
    }
}

blaze_sim::kind_test! {
    /// sqlite3.h read and the amalgamation compiled.
    process fn rung_sqlite() {
        climb(&SQLITE);
    }
}

blaze_sim::kind_test! {
    process fn rung_rocksdb() {
        climb(&ROCKSDB);
    }
}

blaze_sim::kind_test! {
    process fn rung_blst() {
        climb(&BLST);
    }
}

blaze_sim::kind_test! {
    process fn rung_aws_lc() {
        climb(&AWS_LC);
    }
}

blaze_sim::kind_test! {
    process fn rung_c_kzg() {
        climb(&C_KZG);
    }
}

blaze_sim::kind_test! {
    process fn rung_cpython() {
        climb(&CPYTHON);
    }
}

blaze_sim::kind_test! {
    process fn rung_numpy() {
        climb(&NUMPY);
    }
}
