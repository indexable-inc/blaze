//! Every crate of the pinned standard library keyed from its real sources, as the no-daemon
//! command keys them: std's graph as the load resolves it (`blaze_units::stdlib::cfg::std_graph`,
//! `-Zbuild-std`'s defaults for a dev build on the host), each library unit's cfg and variables
//! from the one source the command's graph takes them from (`stdlib::cfg::std_cfg`: the target's
//! cfg, the profile, the features the load resolved, the ported script's output), its files and
//! late answers by the one gather (`crate::gather::gather`, through a disk [`Source`]), then
//! `unit::key`. Nothing of a unit's cfg, features or files is written here by hand. One debug run
//! names every file and item of std that keying refuses, where one release build of the command
//! finds one per run.
//!
//! The library is found as blaze finds it (blaze-cargo's `toolchain::library`): the directory
//! `BLAZE_LIBRARY` names, else the `library/` of the tree this blaze is built from, which is the
//! one its `DIGEST` is compiled in from. A library that is not there fails the test, naming the
//! path it looked at: it is never skipped. Every file of it is handed to the std graph's tree
//! (the load reads manifests and asks which files are there; a test reads the whole directory
//! once rather than mirror which).
//!
//! The invariant: no file of std images with two rows of one stable id (all rows, every cfg),
//! and every library unit of std's graph keys under its own cfg. The control is
//! `tests::equal_siblings_have_their_own_ids`: the planted rules fail on text std has.

use std::{
    alloc::Global,
    fmt::Write as _,
    path::{Path, PathBuf},
};

use blaze_ids::ContentId;
use blaze_keyed::{
    kind::{FileImage, Key, Namespace, Settings, ToolId},
    record::StableId,
};
use blaze_units::{query::StdGraph, stdlib};
use cfg::CfgOptions;
use intern::Symbol;
use syntax::{Edition, Extensions};

use crate::{
    KeyError,
    gather::{self, Gather, ImageList, Images, Source, SourceError, UnitText},
    image::Reading,
    read::Image,
    unit::{self, LateAnswer, Unit, UnitFile},
};

/// What blaze's locator reads first (blaze-cargo `toolchain::library::VAR`).
const LIBRARY_VAR: &str = "BLAZE_LIBRARY";

const TOOL: ToolId = ToolId { tool: ContentId([7; 32]), target: ContentId([8; 32]), namespace: Namespace::PUBLIC };

/// The library's directory, or the test fails naming where it looked.
fn library() -> PathBuf {
    let dir = match std::env::var_os(LIBRARY_VAR).filter(|it| !it.is_empty()) {
        Some(named) => PathBuf::from(named),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../library"),
    };
    let marker = dir.join("core/src/lib.rs");
    assert!(
        marker.is_file(),
        "the standard library's sources are not at {}: it has no {} ({LIBRARY_VAR} names another `library/` directory)",
        dir.display(),
        marker.display()
    );
    // Normal, as the gather names every file it reaches (`mods::normal`).
    dir.canonicalize().unwrap_or_else(|error| panic!("{}: {error}", dir.display()))
}

/// Every file under `dir`, by its path under `library` (`/` separated), with its bytes.
fn files_under(library: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>), Global>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    for entry in entries {
        let path = entry.map_err(|error| format!("{}: {error}", dir.display()))?.path();
        let meta = std::fs::metadata(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        if meta.is_dir() {
            files_under(library, &path, out)?;
            continue;
        }
        let relative = path.strip_prefix(library).map_err(|_| format!("{} is not under {}", path.display(), library.display()))?;
        let named = relative.to_str().ok_or_else(|| format!("{}: not UTF-8", path.display()))?;
        let bytes = std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        out.push((named.to_owned(), bytes));
    }
    Ok(())
}

/// The host's target tuple, as the command names it (`blaze_command::front::host`).
fn host() -> &'static str {
    match (cfg!(target_arch = "aarch64"), cfg!(target_os = "macos"), cfg!(target_os = "linux")) {
        (true, true, _) => "aarch64-apple-darwin",
        (true, _, true) => "aarch64-unknown-linux-gnu",
        (false, _, true) => "x86_64-unknown-linux-gnu",
        _ => panic!("the test runs on a host blaze has a target for (aarch64 macOS, aarch64 or x86-64 Linux)"),
    }
}

/// The cfg lines (`flag`, `key="value"`) as options, as the command's `front::cfg_of` reads
/// them.
fn options_of(lines: &[String]) -> CfgOptions {
    let mut options = CfgOptions::default();
    for line in lines {
        match line.split_once('=') {
            None => options.insert_atom(Symbol::intern(line.trim())),
            Some((key, value)) => options.insert_key_value(Symbol::intern(key.trim()), Symbol::intern(value.trim().trim_matches('"'))),
        }
    }
    options
}

/// The disk, for the library's files.
struct Disk;

impl Source for Disk {
    fn read<B: core::alloc::Allocator>(&mut self, path: &str, into: &mut Vec<u8, B>) -> Result<bool, SourceError> {
        into.clear();
        match std::fs::read(path) {
            Ok(bytes) => {
                into.extend_from_slice(&bytes);
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err("the std test: a library file does not read"),
        }
    }

    fn exists(&mut self, path: &str) -> Result<bool, SourceError> {
        match std::fs::metadata(path) {
            Ok(meta) => Ok(meta.is_file()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err("the std test: a library path does not stat"),
        }
    }
}

/// The test's own digests of late answers: which variable or file is read, and whether it is
/// there, are the gather's; only the hash is the test's.
struct TestDigests;

impl gather::Digests for TestDigests {
    fn var(value: Option<&[u8]>) -> ContentId {
        match value {
            Some(value) => ContentId::of("blaze-unitkey std test variable", value),
            None => ContentId::of("blaze-unitkey std test absent variable", b""),
        }
    }

    fn file(bytes: &[u8]) -> ContentId {
        ContentId::of("blaze-unitkey std test file", bytes)
    }
}

/// A variable row as the gather reads it.
fn pair(row: &(String, String)) -> (&[u8], &[u8]) {
    (row.0.as_bytes(), row.1.as_bytes())
}

/// The line (from 1) row `row` of `image` starts on.
fn line_of(image: &Image<'_>, row: u32) -> usize {
    let Some(item) = image.items().get(row as usize) else { return 0 };
    let start = image.tokens().get(item.tokens.at as usize).map_or(0, |token| token.start);
    image.lines().partition_point(|line| *line <= start)
}

/// Every row of the file at `path`, module `module`, image `bytes` under `key`, has its own
/// stable id, under every cfg: the image's rule.
fn own_ids(path: &str, module: &[u8], bytes: &[u8], key: &Key<FileImage>, out: &mut String) {
    let Ok(image) = Image::open(bytes, key) else {
        let _ = writeln!(out, "{path}: its image does not open");
        return;
    };
    let mut ids: Vec<(StableId, u32), Global> = Vec::with_capacity_in(image.items().len(), Global);
    for row in 0..image.items().len() as u32 {
        match image.stable_id(module, row) {
            Ok(id) => ids.push((id, row)),
            Err(error) => {
                let _ = writeln!(out, "{path}: row {row}: {error:?}");
                return;
            }
        }
    }
    ids.sort_unstable();
    for pair in ids.windows(2).filter(|pair| pair[0].0 == pair[1].0) {
        let item = image.items().get(pair[0].1 as usize).and_then(|item| image.str(item.path).ok()).unwrap_or(&[][..]);
        let _ = writeln!(
            out,
            "{path}:{} and :{}: two rows `{}` of one stable id",
            line_of(&image, pair[0].1),
            line_of(&image, pair[1].1),
            String::from_utf8_lossy(item)
        );
    }
}

/// Crate `name`'s gathered `text` keyed under `options`; a refusal named with its files and
/// lines into `out`.
fn key_crate(name: &str, text: &UnitText<Global>, images: &ImageList<Global>, options: &CfgOptions, out: &mut String) {
    let mut files = Vec::with_capacity_in(text.files.len(), Global);
    for file in text.files.iter() {
        let Some(image) = images.get(&file.image) else {
            let _ = writeln!(out, "`{name}`: {}: its image was not filed", String::from_utf8_lossy(&file.path));
            return;
        };
        files.push(UnitFile { key: file.image, image, module: &file.module });
    }
    let mut answers = Vec::with_capacity_in(text.answers.len(), Global);
    answers.extend(text.answers.iter().map(|answer| LateAnswer { file: answer.file, late: answer.late, name: &answer.name, value: answer.value }));
    let identity = ContentId::of("blaze-unitkey std test unit", name.as_bytes());
    let settings = Settings(ContentId::of("blaze-unitkey std test settings", b""));
    // No printed names: this test keys the unit, it prints nothing.
    let mut names = Vec::with_capacity_in(files.len(), Global);
    names.resize(files.len(), ContentId([0; 32]));
    let input = Unit { tool: TOOL, settings, unit: identity, cfg: options, files: &files, answers: &answers, names: &names };
    match unit::key(&input, Global) {
        Ok(_) => {}
        Err(KeyError::TwoItems { path, first, second, .. }) => {
            let place = |at: crate::ItemAt| {
                let file = &files[at.file as usize];
                let line = Image::open(file.image, &file.key).map_or(0, |image| line_of(&image, at.row));
                format!("{}:{line}", String::from_utf8_lossy(&text.files[at.file as usize].path))
            };
            let _ = writeln!(
                out,
                "`{name}`: two items `{}` of one stable id, at {} and {}",
                String::from_utf8_lossy(&path),
                place(first),
                place(second)
            );
        }
        Err(error) => {
            let _ = writeln!(out, "`{name}`: {error}");
        }
    }
}

/// One file of each crate that a `mod` in a macro call's tokens names, on every host
/// (`mods`, "Modules a macro call writes"): core's `cfg_select!` with a `#[path]`
/// (`core/src/io/io_slice.rs:13`), compiler_builtins' `cfg_select_nofmt!` with a visibility
/// (`compiler-builtins/libm/src/math/mod.rs:75`), libc's own `cfg_if!` (`src/lib.rs:238`).
const IN_CALL_NAMED: [(&str, &str); 3] = [
    ("core", "core/src/io/io_slice/repr_iovec.rs"),
    ("compiler_builtins", "compiler-builtins/libm/src/math/generic/mod.rs"),
    ("libc", "vendor/libc-0.2.191/src/unix/mod.rs"),
];

/// Every file hir-def's std census (aarch64-unknown-linux-gnu, job census1 at this pin) found
/// read from disk because a `mod` a macro call wrote named it, or a module under one: each
/// must be among its crate's gathered files on that host (83 distinct: compiler_builtins 26,
/// core 1, libc 56). Relative to `library/`.
const IN_CALL_CENSUS: [(&str, &str); 83] = [
    ("compiler_builtins", "compiler-builtins/libm/src/math/arch/aarch64/fma.rs"),
    ("compiler_builtins", "compiler-builtins/libm/src/math/arch/aarch64/mod.rs"),
    ("compiler_builtins", "compiler-builtins/libm/src/math/arch/aarch64/rounding.rs"),
    ("compiler_builtins", "compiler-builtins/libm/src/math/arch/aarch64/sqrt.rs"),
    ("compiler_builtins", "compiler-builtins/libm/src/math/generic/ceil.rs"),
    ("compiler_builtins", "compiler-builtins/libm/src/math/generic/copysign.rs"),
    ("compiler_builtins", "compiler-builtins/libm/src/math/generic/fabs.rs"),
    ("compiler_builtins", "compiler-builtins/libm/src/math/generic/fdim.rs"),
    ("compiler_builtins", "compiler-builtins/libm/src/math/generic/floor.rs"),
    ("compiler_builtins", "compiler-builtins/libm/src/math/generic/fma.rs"),
    ("compiler_builtins", "compiler-builtins/libm/src/math/generic/fma_wide.rs"),
    ("compiler_builtins", "compiler-builtins/libm/src/math/generic/fmax.rs"),
    ("compiler_builtins", "compiler-builtins/libm/src/math/generic/fmaximum.rs"),
    ("compiler_builtins", "compiler-builtins/libm/src/math/generic/fmaximum_num.rs"),
    ("compiler_builtins", "compiler-builtins/libm/src/math/generic/fmin.rs"),
    ("compiler_builtins", "compiler-builtins/libm/src/math/generic/fminimum.rs"),
    ("compiler_builtins", "compiler-builtins/libm/src/math/generic/fminimum_num.rs"),
    ("compiler_builtins", "compiler-builtins/libm/src/math/generic/fmod.rs"),
    ("compiler_builtins", "compiler-builtins/libm/src/math/generic/frexp.rs"),
    ("compiler_builtins", "compiler-builtins/libm/src/math/generic/ilogb.rs"),
    ("compiler_builtins", "compiler-builtins/libm/src/math/generic/mod.rs"),
    ("compiler_builtins", "compiler-builtins/libm/src/math/generic/rint.rs"),
    ("compiler_builtins", "compiler-builtins/libm/src/math/generic/round.rs"),
    ("compiler_builtins", "compiler-builtins/libm/src/math/generic/scalbn.rs"),
    ("compiler_builtins", "compiler-builtins/libm/src/math/generic/sqrt.rs"),
    ("compiler_builtins", "compiler-builtins/libm/src/math/generic/trunc.rs"),
    ("core", "core/src/io/io_slice/repr_iovec.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/glibc/bits/signum_generic.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/glibc/io/sys/mod.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/glibc/io/sys/statvfs.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/glibc/mod.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/glibc/posix/unistd.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/glibc/signal.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/glibc/socket/mod.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/glibc/socket/sys/socket.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/glibc/sys.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/glibc/sysdeps/nptl/bits/struct_mutex.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/glibc/sysdeps/nptl/mod.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/glibc/sysdeps/nptl/pthread.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/glibc/sysdeps/unix/linux/bits/sigaction.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/glibc/sysdeps/unix/linux/bits/signum_arch.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/glibc/sysdeps/unix/linux/bits/socket.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/glibc/sysdeps/unix/linux/bits/socket_type.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/glibc/sysdeps/unix/linux/bits/statvfs.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/glibc/sysdeps/unix/linux/bits/types/mod.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/glibc/sysdeps/unix/linux/bits/types/siginfo_t.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/glibc/sysdeps/unix/linux/mod.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/glibc/sysdeps/unix/linux/net/route.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/glibc/sysdeps/unix/mod.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/linux_uapi/asm/mod.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/linux_uapi/asm/socket.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/linux_uapi/linux/can.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/linux_uapi/linux/can/bcm.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/linux_uapi/linux/can/error.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/linux_uapi/linux/can/j1939.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/linux_uapi/linux/can/netlink.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/linux_uapi/linux/can/raw.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/linux_uapi/linux/futex.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/linux_uapi/linux/if_addr.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/linux_uapi/linux/if_link.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/linux_uapi/linux/if_packet.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/linux_uapi/linux/keyctl.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/linux_uapi/linux/membarrier.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/linux_uapi/linux/mod.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/linux_uapi/linux/mount.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/linux_uapi/linux/netlink.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/linux_uapi/linux/pidfd.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/linux_uapi/linux/sctp.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/linux_uapi/linux/tls.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/linux_uapi/linux/types.rs"),
    ("libc", "vendor/libc-0.2.191/src/new/linux_uapi/mod.rs"),
    ("libc", "vendor/libc-0.2.191/src/primitives.rs"),
    ("libc", "vendor/libc-0.2.191/src/types.rs"),
    ("libc", "vendor/libc-0.2.191/src/unix/linux_like/linux/arch/generic/mod.rs"),
    ("libc", "vendor/libc-0.2.191/src/unix/linux_like/linux/arch/mod.rs"),
    ("libc", "vendor/libc-0.2.191/src/unix/linux_like/linux/gnu/b64/aarch64/lp64.rs"),
    ("libc", "vendor/libc-0.2.191/src/unix/linux_like/linux/gnu/b64/aarch64/mod.rs"),
    ("libc", "vendor/libc-0.2.191/src/unix/linux_like/linux/gnu/b64/mod.rs"),
    ("libc", "vendor/libc-0.2.191/src/unix/linux_like/linux/gnu/mod.rs"),
    ("libc", "vendor/libc-0.2.191/src/unix/linux_like/linux/mod.rs"),
    ("libc", "vendor/libc-0.2.191/src/unix/linux_like/linux_l4re_shared.rs"),
    ("libc", "vendor/libc-0.2.191/src/unix/linux_like/mod.rs"),
    ("libc", "vendor/libc-0.2.191/src/unix/mod.rs"),
];

/// The files of `IN_CALL_NAMED` (every host) and, on the census's host, of `IN_CALL_CENSUS`
/// that crate `name`'s gathered `text` misses, each a line into `out`.
fn in_call_missing(library: &Path, name: &str, text: &UnitText<Global>, out: &mut String) {
    let census = host() == "aarch64-unknown-linux-gnu";
    let lists: [&[(&str, &str)]; 2] = [&IN_CALL_NAMED, if census { &IN_CALL_CENSUS[..] } else { &[] }];
    for &(krate, relative) in lists.iter().flat_map(|list| list.iter()) {
        if krate != name {
            continue;
        }
        let path = library.join(relative);
        let spelled = path.to_str().unwrap_or("").as_bytes();
        if !text.files.iter().any(|file| file.path[..] == *spelled) {
            let _ = writeln!(out, "`{name}`: {relative}, which a `mod` in a macro call's tokens names, is not among its files");
        }
    }
}

/// Unit `at` of std's `graph` gathered and keyed, every refusal a line into `out`; whether it
/// keyed.
fn check_unit(
    library: &Path,
    graph: &StdGraph,
    at: usize,
    run: &stdlib::cfg::ScriptRun<'_>,
    images: &mut ImageList<Global>,
    out: &mut String,
) -> bool {
    let unit = &graph.units[at];
    let name = unit.target.name.as_str();
    let before = out.len();
    let refuse = |out: &mut String, why: String| {
        let _ = writeln!(out, "`{name}`: {why}");
        false
    };
    // The unit's cfg and variables as the command's graph gives them.
    let std = match stdlib::cfg::std_cfg(&graph.units, &graph.plans, &graph.packages, at, host(), run) {
        Ok(std) => std,
        Err(why) => return refuse(out, format!("its cfg: {why}")),
    };
    let options = options_of(&std.lines);
    let mut env: Vec<(String, String), Global> = Vec::new_in(Global);
    env.extend(graph.plans[at].env.iter().cloned());
    env.extend(std.env.iter().cloned());
    env.sort();
    if let Some(pair) = env.windows(2).find(|pair| pair[0].0 == pair[1].0) {
        return refuse(out, format!("`{}` is set to `{}` and to `{}`", pair[0].0, pair[0].1, pair[1].1));
    }
    let Ok(edition) = unit.target.edition.parse::<Edition>() else {
        return refuse(out, format!("edition `{}`", unit.target.edition));
    };
    let Some(named) = unit.target.src_path.to_str() else {
        return refuse(out, format!("{}: not UTF-8", unit.target.src_path.display()));
    };
    let root = match stdlib::cfg::on_disk(named, run) {
        Ok(root) => root,
        Err(why) => return refuse(out, why.to_string()),
    };
    let Some(root) = root.to_str() else {
        return refuse(out, format!("{}: not UTF-8", root.display()));
    };
    let input = Gather {
        tool: TOOL,
        reading: Reading { edition, extensions: Extensions::NONE },
        cfg: &options,
        root: root.as_bytes(),
        env: &env,
        pair,
        member: false,
        defer: true,
    };
    let text = match gather::gather::<_, _, TestDigests, _, _>(&input, &mut Disk, images, Global) {
        Ok(text) => text,
        Err(error) => {
            let why = format!("gathering: {}: {} ({})", String::from_utf8_lossy(&error.path), String::from_utf8_lossy(&error.what), error.rule);
            return refuse(out, why);
        }
    };
    in_call_missing(library, name, &text, out);
    for file in text.files.iter() {
        let path = String::from_utf8_lossy(&file.path);
        match images.get(&file.image) {
            Some(bytes) => own_ids(&path, &file.module, bytes, &file.image, out),
            None => {
                let _ = writeln!(out, "{path}: its image was not filed");
            }
        }
    }
    key_crate(name, &text, images, &options, out);
    out.len() == before
}

/// The invariant: every library unit of std's graph keys from its real sources, and no file of
/// it images with two rows of one id; every file a `mod` in a macro call's tokens names is
/// among its crate's files (`in_call_missing`). Every refusal of every crate is listed in the
/// one failure.
#[test]
fn every_crate_of_the_pinned_std_keys() {
    let library = library();
    let mut files: Vec<(String, Vec<u8>), Global> = Vec::new_in(Global);
    files_under(&library, &library, &mut files).unwrap_or_else(|why| panic!("reading the library: {why}"));
    let named: Vec<(&str, &[u8])> = files.iter().map(|(path, bytes)| (path.as_str(), &bytes[..])).collect();
    let graph = stdlib::cfg::std_graph(&named, |mount| stdlib::cfg::dev_key(mount, host()))
        .unwrap_or_else(|why| panic!("std's graph from {}: {why}", library.display()));
    let upstream = named.iter().find(|(path, _)| *path == "UPSTREAM").map(|(_, bytes)| String::from_utf8_lossy(bytes)).expect("the graph read UPSTREAM");
    let commit = stdlib::library_commit(&upstream).unwrap_or_else(|why| panic!("{}/UPSTREAM: {why}", library.display()));
    let named_at = stdlib::library_at(&commit);
    // blaze's stand-ins for `rustc` and `rustdoc` are named in a script's environment, never
    // run: the scripts are ported.
    let tools = std::env::temp_dir().join("blaze-unitkey-std-test-tools");
    let run = stdlib::cfg::ScriptRun { library: &named_at, dir: &library, host: host(), tools: &tools, inherited: &[], jobs: 1 };
    let mut images = ImageList::new_in(Global);
    let mut refused = String::new();
    let (mut keyed, mut units) = (0, 0);
    for at in 0..graph.units.len() {
        if !graph.units[at].target.is_lib() {
            continue;
        }
        units += 1;
        keyed += usize::from(check_unit(&library, &graph, at, &run, &mut images, &mut refused));
    }
    assert!(units > 0, "std's graph at {} has no library unit", library.display());
    assert!(refused.is_empty(), "{keyed} of {units} library units of std at {} keyed; refused:\n{refused}", library.display());
}
