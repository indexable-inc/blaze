//! debug_side: a dev build's executable carries no debug info, its side file is found unaided,
//! and a panic's backtrace names the same functions and file:line both ways. The port of
//! `harness/v2/debug_side.py`, to be deleted once both were run on the same tree and agreed
//! (docs/v2/plan/design/scripts.md, "Ports and their parity"). `DOC` below is what its usage
//! error prints.
//!
//!     blaze run -q -p scripts --bin debug_side -- '{"engine": {"kind": "v2", "blaze": "target/release/blaze"}, "out": "/tmp/debug-side.json"}'
//!     blaze run -q -p scripts --bin debug_side -- selftest
//!
//! What the Python let through as a traceback (an `OSError`, a `ValueError` of a reader, a
//! `struct.error` or `KeyError` of a short or odd file) ends this one with a `fail` line naming
//! it, exit status 1, as the traceback did; no report is written either way.

use std::path::{Path, PathBuf};

use blaze_script::{
    Value,
    engine::{self, Engine, EngineSpec, Fault, last_chars},
    expect, fail,
    input::{self, Fields, Input, InputError},
    pyfmt::{repr_bytes, repr_str},
    pystr::splitlines,
    regex::Regex,
    report::{Number, Report, Verdict},
    which,
};

/// What the usage error prints: harness/v2/debug_side.py's docstring.
const DOC: &str = r#"debug_side.py: a dev build's executable carries no debug info, its side file is found unaided,
and a panic's backtrace names the same functions and file:line as before
(docs/v2/plan/design/debug-side-file.md).

    uv run --frozen python harness/v2/debug_side.py '{"engine": {"kind": "v2", "blaze": "target/release/blaze"}, "out": "/tmp/debug-side.json"}'
    uv run --frozen python harness/v2/debug_side.py selftest

The input is one JSON object (inline, a file's path, or `-`):

    engine   the blaze measured (engine.EngineSpec)
    out      the result's path (report.py's JSON)
    work     where the program and its daemon's home go; a new directory under /tmp when empty
    keep     keep the work directory (false)

It writes a hello world whose `main` calls `deep`, which panics at a known line, and builds it
twice with a daemon and an empty home each: once as it is (debug info beside the executable, the
default), once with `split-debuginfo = "off"` in `[profile.dev]` (debug info inside). Outcomes:

    sizes        the executable's and the side file's bytes, both ways; fail when the default's
                 executable has a `.debug_*` (ELF) or `__DWARF` (Mach-O) section, or no side file
                 is beside it at each path the executable is at
    build id     the side file carries the executable's build id (ELF: the note; Mach-O: LC_UUID)
                 and, on ELF, has the CRC-32 the executable's `.gnu_debuglink` names
    backtrace    the program run with RUST_BACKTRACE=1 exits 101 and its backtrace has `deep` and
                 `main` frames at `src/main.rs:LINE`, the same frames (function and file:line) in
                 both builds
    gdb, lldb    `info line deep` / `image lookup -n deep -v` resolves `src/main.rs`; "not checked:
                 no gdb" (unmeasured) where the debugger is not installed: never a silent skip

`selftest` checks the backtrace reader, the section readers and the CRC without blaze.
"#;

const SCRIPT: &str = "debug_side.py";
const PANIC_LINE: u64 = 6;
const MAIN: &str = r#"fn main() {
    deep(std::env::args().count());
}

#[inline(never)]
fn deep(n: usize) { panic!("debug side {n}"); }
"#;
const MANIFEST: &str = r#"[package]
name = "debugside"
version = "0.1.0"
edition = "2021"

[workspace]
"#;
const INSIDE: &str = r#"
[profile.dev]
split-debuginfo = "off"
"#;
const FRAME: &str = r"^\s*\d+:\s+(?:0x[0-9a-f]+ - )?(?P<function>\S.*?)\s*$";
const AT: &str = r"^\s+at (?P<file>\S+?):(?P<line>\d+)(?::\d+)?\s*$";
const HASH: &str = r"::h[0-9a-f]{16}$";

struct Args {
    engine: EngineSpec,
    out: String,
    work: String,
    keep: bool,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["engine", "out", "work", "keep"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        Ok(Args {
            engine: f.object_required("engine")?,
            out: f.str("out", None)?,
            work: f.str("work", Some(""))?,
            keep: f.bool("keep", Some(false))?,
        })
    }
}

/// One build: the executable at each path it is at, and what running it printed.
struct Built {
    executables: Vec<PathBuf>,
    stderr: String,
    status: i32,
}

/// A backtrace frame with a place: (function, file, line).
type Frame = (String, String, u64);

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|e| panic!("{pattern}: {e}"))
}

/// A `RUST_BACKTRACE=1` backtrace's frames that have a place: (function, file, line), the
/// function without its hash suffix.
fn frames(stderr: &str) -> Vec<Frame> {
    let (frame, at, hash) = (regex(FRAME), regex(AT), regex(HASH));
    let mut out = Vec::new();
    let mut function: Option<String> = None;
    for line in splitlines(stderr) {
        if let Some(m) = frame.captures(line) {
            function = Some(hash.replace(&m["function"], "").into_owned());
        } else if let Some(m) = at.captures(line)
            && let Some(name) = function.take()
        {
            // `int(...)` of the digits: a number past u64 is the one thing this reads otherwise.
            out.push((name, m["file"].to_owned(), m["line"].parse().unwrap_or(u64::MAX)));
        }
    }
    out
}

/// The frames in the program's own source.
fn own_frames(stderr: &str) -> Vec<Frame> {
    frames(stderr).into_iter().filter(|it| it.1.ends_with("src/main.rs")).collect()
}

/// `repr` of a list of frames, as Python prints a list of `(str, str, int)` tuples.
fn repr_frames(frames: &[Frame]) -> String {
    let each: Vec<String> =
        frames.iter().map(|(f, file, line)| format!("({}, {}, {line})", repr_str(f), repr_str(file))).collect();
    format!("[{}]", each.join(", "))
}

/// An exception the Python let through, said as its traceback's last line would.
fn raised(kind: &str, said: &str) -> Fault {
    Fault::Os(format!("{kind}: {said}"))
}

/// `data[at..at + n]`, cut at the end as a Python slice is.
fn slice(data: &[u8], at: u64, n: u64) -> &[u8] {
    let len = data.len() as u64;
    let start = at.min(len) as usize;
    let end = at.saturating_add(n).min(len) as usize;
    &data[start..end.max(start)]
}

/// `struct.unpack_from("<I"/"<H"/"<Q", data, at)`: the little-endian integer of `N` bytes.
fn le<const N: usize>(data: &[u8], at: u64) -> Result<u64, Fault> {
    let bytes = usize::try_from(at).ok().and_then(|at| data.get(at..at.checked_add(N)?));
    match bytes {
        Some(b) => Ok(b.iter().rev().fold(0, |acc, &x| (acc << 8) | u64::from(x))),
        None => Err(raised(
            "struct.error",
            &format!("unpack_from requires a buffer of at least {} bytes for unpacking {N} bytes at offset {at} (actual buffer size is {})", at.saturating_add(N as u64), data.len()),
        )),
    }
}

/// `b[from:b.index(b"\0", from)].decode()`.
fn c_string(b: &[u8], from: usize) -> Result<String, Fault> {
    let rest = b.get(from..).unwrap_or_default();
    let Some(end) = rest.iter().position(|&c| c == 0) else {
        return Err(raised("ValueError", "subsection not found"));
    };
    String::from_utf8(rest[..end].to_vec()).map_err(|e| raised("UnicodeDecodeError", &e.to_string()))
}

/// An ELF64 little-endian file's sections by name: (file offset, size). A name twice keeps
/// the last, as the Python's dict did; the names come out sorted, which is all `carried` asks.
fn elf_sections(data: &[u8]) -> Result<std::collections::BTreeMap<String, (u64, u64)>, Fault> {
    if data.get(..6) != Some(b"\x7fELF\x02\x01".as_slice()) {
        return Err(raised("ValueError", "not a 64-bit little-endian ELF file"));
    }
    let shoff = le::<8>(data, 0x28)?;
    let (entsize, count, strndx) = (le::<2>(data, 0x3A)?, le::<2>(data, 0x3C)?, le::<2>(data, 0x3E)?);
    let mut headers = Vec::new();
    for n in 0..count {
        let at = shoff.saturating_add(n * entsize);
        let field = |off: u64| at.saturating_add(off);
        headers.push((le::<4>(data, at)?, le::<4>(data, field(4))?, le::<8>(data, field(24))?, le::<8>(data, field(32))?));
    }
    let Some(&(_, _, names_at, names_len)) = headers.get(strndx as usize) else {
        return Err(raised("IndexError", "list index out of range"));
    };
    let names = slice(data, names_at, names_len);
    let mut out = std::collections::BTreeMap::new();
    for (name, kind, offset, size) in headers {
        if kind != 0 {
            out.insert(c_string(names, name as usize)?, (offset, size));
        }
    }
    Ok(out)
}

/// A 64-bit Mach-O file's load commands: (command, its bytes).
fn macho_commands(data: &[u8]) -> Result<Vec<(u64, &[u8])>, Fault> {
    let magic = le::<4>(data, 0)?;
    let count = le::<4>(data, 16)?;
    le::<4>(data, 20)?;
    if magic != 0xFEED_FACF {
        return Err(raised("ValueError", "not a 64-bit Mach-O file"));
    }
    let mut out = Vec::new();
    let mut at = 32;
    for _ in 0..count {
        let (command, size) = (le::<4>(data, at)?, le::<4>(data, at + 4)?);
        out.push((command, slice(data, at, size)));
        at = at.saturating_add(size);
    }
    Ok(out)
}

fn macho_uuid(data: &[u8]) -> Result<Option<Vec<u8>>, Fault> {
    Ok(macho_commands(data)?.into_iter().find(|(command, _)| *command == 0x1B).map(|(_, body)| slice(body, 8, 16).to_vec()))
}

fn macho_segments(data: &[u8]) -> Result<Vec<String>, Fault> {
    let mut out = Vec::new();
    for (command, body) in macho_commands(data)? {
        if command == 0x19 {
            let name = slice(body, 8, 16);
            let end = name.iter().rposition(|&c| c != 0).map_or(0, |at| at + 1);
            out.push(String::from_utf8(name[..end].to_vec()).map_err(|e| raised("UnicodeDecodeError", &e.to_string()))?);
        }
    }
    Ok(out)
}

fn is_elf(data: &[u8]) -> bool {
    data.starts_with(b"\x7fELF")
}

/// The sections or segments of an executable that hold debug info.
fn carried(data: &[u8]) -> Result<Vec<String>, Fault> {
    if is_elf(data) {
        return Ok(elf_sections(data)?.into_keys().filter(|name| name.starts_with(".debug_")).collect());
    }
    Ok(macho_segments(data)?.into_iter().filter(|name| name == "__DWARF").collect())
}

/// Where the executable's side file is looked for: beside it, under the name its
/// `.gnu_debuglink` holds (ELF), or in its `.dSYM` bundle (Mach-O).
fn side_of(exe: &Path, data: &[u8]) -> Result<PathBuf, Fault> {
    let parent = exe.parent().unwrap_or(Path::new(""));
    let name = exe.file_name().map(|it| it.to_string_lossy().into_owned()).unwrap_or_default();
    if is_elf(data) {
        let Some(&(at, size)) = elf_sections(data)?.get(".gnu_debuglink") else {
            return Err(raised("ValueError", "the executable has no .gnu_debuglink"));
        };
        return Ok(parent.join(c_string(slice(data, at, size), 0)?));
    }
    Ok(parent.join(format!("{name}.dSYM")).join("Contents/Resources/DWARF").join(name))
}

/// zlib's CRC-32 (the reflected polynomial 0xEDB88320), as `zlib.crc32` computes it.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|c| format!("{c:02x}")).collect()
}

/// Why `side` is not `exe`'s side file; empty when it is.
fn same_build(exe: &[u8], side: &[u8]) -> Result<String, Fault> {
    const NOTE: &str = ".note.gnu.build-id";
    if is_elf(exe) {
        let (of_exe, of_side) = (elf_sections(exe)?, elf_sections(side)?);
        let (Some(&(ea, es)), Some(&(sa, ss))) = (of_exe.get(NOTE), of_side.get(NOTE)) else {
            return Ok("a build id note is missing".to_owned());
        };
        let (note_exe, note_side) = (slice(exe, ea, es), slice(side, sa, ss));
        if note_exe != note_side {
            return Ok(format!(
                "the side file's build id {} is not the executable's {}",
                hex(note_side),
                hex(note_exe)
            ));
        }
        let Some(&(at, size)) = of_exe.get(".gnu_debuglink") else {
            return Err(raised("KeyError", "'.gnu_debuglink'"));
        };
        // `link[0] + link[1] - 4`: a Python int, negative for a link shorter than 4 bytes, which
        // unpack_from then reads from the end. Not a link blaze writes.
        let crc = le::<4>(exe, at.wrapping_add(size).wrapping_sub(4))?;
        let got = u64::from(crc32(side));
        if got != crc {
            return Ok(format!("the side file's CRC-32 {got:#x} is not the {crc:#x} the executable's .gnu_debuglink names"));
        }
        return Ok(String::new());
    }
    let (of_exe, of_side) = (macho_uuid(exe)?, macho_uuid(side)?);
    if of_exe.is_none() || of_exe != of_side {
        let shown = |it: &Option<Vec<u8>>| it.as_deref().map_or("None".to_owned(), repr_bytes);
        return Ok(format!("the side file's LC_UUID {} is not the executable's {}", shown(&of_side), shown(&of_exe)));
    }
    Ok(String::new())
}

/// Every path under `top` named `name`, as `top.rglob(name)` walks (into no symlinked
/// directory), that is a file.
fn rglob_files(top: &Path, name: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut dirs = vec![top.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if entry.file_type().is_ok_and(|it| it.is_dir()) {
                dirs.push(path.clone());
            }
            if entry.file_name() == name && path.is_file() {
                out.push(path);
            }
        }
    }
    out
}

/// `blaze build` of the program with `manifest`, with an empty home and a daemon of its own,
/// then the program run with a backtrace.
fn build(eng: &Engine, top: &Path, label: &str, manifest: &str) -> Result<Built, Fault> {
    let (tree, home) = (top.join(label).join("src"), top.join(label).join("home"));
    let _ = std::fs::remove_dir_all(top.join(label));
    let src = tree.join("src");
    std::fs::create_dir_all(&src).map_err(engine::os_at(&src))?;
    let toml = tree.join("Cargo.toml");
    std::fs::write(&toml, manifest).map_err(engine::os_at(&toml))?;
    let main = src.join("main.rs");
    std::fs::write(&main, MAIN).map_err(engine::os_at(&main))?;
    let env = eng.env(&home, None, &[])?;
    let built = (|| {
        let ran = eng.build(&format!("{label}: build"), &tree, &env, &[], "build", 7200.0)?;
        if ran.returncode != 0 {
            return Err(Fault::Failed(format!("{label}: `blaze build` exited {}:\n{}", ran.returncode, ran.said(3000))));
        }
        if let Some(who) = eng.not_blazes(&ran) {
            return Err(Fault::Failed(format!("{label}: built by {who}, not blaze")));
        }
        eng.build_dir(&tree, &env)
    })();
    // The Python's `finally`: the daemon is stopped either way, and a failure to stop it is
    // what is raised.
    eng.stop(&tree, &env)?;
    let target = built?;
    let mut found: Vec<PathBuf> = rglob_files(&target, "debugside")
        .into_iter()
        .filter(|path| {
            let parts: Vec<_> = path.components().map(|it| it.as_os_str().to_owned()).collect();
            // `"debug" in path.parts`: any part of the whole path, as the Python looks.
            !parts.iter().any(|it| it == ".dSYM") && parts.iter().any(|it| it == "debug")
        })
        .collect();
    found.sort();
    let Some(first) = found.first() else {
        return Err(Fault::Failed(format!("{label}: no executable `debugside` under {}", target.display())));
    };
    let top_level = found.iter().find(|path| path.parent().and_then(Path::file_name).is_some_and(|n| n == "debug")).unwrap_or(first);
    let mut env = env.clone();
    env.insert("RUST_BACKTRACE".to_owned(), "1".to_owned());
    let program = engine::run(&format!("{label}: run"), &[top_level.display().to_string()], &tree, &env, 3600.0, false)?;
    Ok(Built { executables: found, stderr: program.stderr, status: program.returncode })
}

/// `argv` on the executable resolves `deep` to `src/main.rs`; unmeasured, saying so, where the
/// debugger is not installed.
fn debugger(rep: &mut Report, name: &str, argv: &[&str], exe: &Path, cwd: &Path) -> Result<(), Fault> {
    if which(argv[0]).is_none() {
        rep.add(name, Verdict::Unmeasured, &format!("not checked: no {}", argv[0]), Vec::new());
        return Ok(());
    }
    let mut words: Vec<String> = argv.iter().map(|it| (*it).to_owned()).collect();
    words.push(exe.display().to_string());
    let ran = engine::run(name, &words, cwd, &engine::scrubbed(), 120.0, false)?;
    let said = format!("{}{}", ran.stdout, ran.stderr);
    if said.contains("src/main.rs") && said.contains(&PANIC_LINE.to_string()) {
        rep.add(name, Verdict::Pass, &format!("`deep` resolves to src/main.rs:{PANIC_LINE}"), Vec::new());
    } else {
        rep.add(
            name,
            Verdict::Fail,
            &format!("`{}` did not resolve `deep` to src/main.rs:{PANIC_LINE}:\n{}", argv.join(" "), last_chars(&said, 2000)),
            Vec::new(),
        );
    }
    Ok(())
}

/// `numbers[key] = value` of a Python dict: a key set again keeps its place.
fn set(numbers: &mut Vec<(String, Number)>, key: String, value: i64) {
    match numbers.iter_mut().find(|(k, _)| *k == key) {
        Some(slot) => slot.1 = Number::Int(value),
        None => numbers.push((key, Number::Int(value))),
    }
}

fn read(path: &Path) -> Result<Vec<u8>, Fault> {
    std::fs::read(path).map_err(|e| Fault::os(&e, path))
}

fn parent_name(path: &Path) -> String {
    path.parent().and_then(Path::file_name).map(|it| it.to_string_lossy().into_owned()).unwrap_or_default()
}

fn measure(eng: &Engine, work: &Path, rep: &mut Report) -> Result<(), Fault> {
    let beside = build(eng, work, "beside", MANIFEST)?;
    let inside = build(eng, work, "inside", &format!("{MANIFEST}{INSIDE}"))?;
    let mut numbers: Vec<(String, Number)> = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    for exe in &beside.executables {
        let data = read(exe)?;
        set(&mut numbers, format!("executable {}", parent_name(exe)), data.len() as i64);
        let debug = carried(&data)?;
        if !debug.is_empty() {
            problems.push(format!("{} carries {}", exe.display(), debug.join(", ")));
        }
        let side = side_of(exe, &data)?;
        if !side.is_file() {
            problems.push(format!("{} has no side file at {}", exe.display(), side.display()));
            continue;
        }
        let size = std::fs::metadata(&side).map_err(|e| Fault::os(&e, &side))?.len();
        set(&mut numbers, format!("side file {}", parent_name(exe)), size as i64);
        let why = same_build(&data, &read(&side)?)?;
        if !why.is_empty() {
            ids.push(format!("{}: {why}", side.display()));
        }
    }
    for exe in &inside.executables {
        let data = read(exe)?;
        set(&mut numbers, format!("executable {}, debug info inside", parent_name(exe)), data.len() as i64);
        if carried(&data)?.is_empty() {
            problems.push(format!("{}, built with split-debuginfo = \"off\", carries no debug info", exe.display()));
        }
    }
    set(&mut numbers, "executables".to_owned(), beside.executables.len() as i64);
    let verdict = |bad: bool| if bad { Verdict::Fail } else { Verdict::Pass };
    let numbers: Vec<(&str, Number)> = numbers.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
    rep.add("sizes", verdict(!problems.is_empty()), &problems.join("\n"), numbers);
    let said = if ids.is_empty() { &problems } else { &ids };
    rep.add("build id", verdict(!ids.is_empty() || !problems.is_empty()), &said.join("\n"), Vec::new());
    let (got, want) = (own_frames(&beside.stderr), own_frames(&inside.stderr));
    let names: Vec<&str> = got.iter().map(|(f, _, _)| f.rsplit("::").next().unwrap_or(f)).collect();
    let placed = got.iter().any(|(f, _, line)| *line == PANIC_LINE && f.ends_with("deep"));
    if beside.status != 101 || inside.status != 101 {
        let detail = format!(
            "the program exited {} and {}, not 101:\n{}",
            beside.status,
            inside.status,
            last_chars(&beside.stderr, 2000)
        );
        rep.add("backtrace", Verdict::Fail, &detail, Vec::new());
    } else if got != want {
        rep.add("backtrace", Verdict::Fail, &format!("beside: {}\ninside: {}", repr_frames(&got), repr_frames(&want)), Vec::new());
    } else if !(names.contains(&"deep") && names.contains(&"main")) || !placed {
        let detail = format!(
            "no `deep` at src/main.rs:{PANIC_LINE} and `main` among {}:\n{}",
            repr_frames(&got),
            last_chars(&beside.stderr, 2000)
        );
        rep.add("backtrace", Verdict::Fail, &detail, Vec::new());
    } else {
        rep.add(
            "backtrace",
            Verdict::Pass,
            &format!("{} frames of src/main.rs, the same both ways", got.len()),
            vec![("frames", Number::Int(got.len() as i64))],
        );
    }
    let exe = beside.executables.last().expect("build finds at least one executable");
    debugger(rep, "gdb", &["gdb", "-batch", "-ex", "info line deep"], exe, work)?;
    debugger(rep, "lldb", &["lldb", "--batch", "-o", "image lookup -n deep -v"], exe, work)
}

fn main_with(args: &Args) -> ! {
    let eng = Engine::of(&args.engine).unwrap_or_else(|f| fail(SCRIPT, f.said()));
    let mut rep = Report::new(SCRIPT, &args.engine, &args.out);
    let work = engine::work_dir(&args.work, "bz-debug-side-").unwrap_or_else(|f| fail(SCRIPT, f.said()));
    if let Err(e) = std::fs::create_dir_all(&work) {
        fail(SCRIPT, Fault::os(&e, &work).said());
    }
    let measured = measure(&eng, &work, &mut rep);
    rep.guard("debug side file", measured);
    if !args.keep {
        let _ = std::fs::remove_dir_all(&work);
    }
    rep.extra.insert("work".to_owned(), Value::String(work.display().to_string()));
    rep.finish()
}

/// An ELF64 file of `sections`, as far as `elf_sections` reads one.
fn test_elf(sections: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut names = vec![0u8];
    for (name, _) in sections {
        names.extend_from_slice(name.as_bytes());
        names.push(0);
    }
    names.extend_from_slice(b".shstrtab\0");
    let find = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).position(|w| w == needle).expect("the name is in the table");
    let mut body = vec![0u8; 64];
    let mut placed = Vec::new();
    let all: Vec<(&str, &[u8])> = sections.iter().map(|(n, d)| (*n, d.as_slice())).chain([(".shstrtab", names.as_slice())]).collect();
    for (name, data) in &all {
        let key = [name.as_bytes(), b"\0"].concat();
        placed.push((find(&names, &key) as u64, body.len() as u64, data.len() as u64));
        body.extend_from_slice(data);
    }
    let shoff = body.len() as u64;
    body.extend_from_slice(&[0; 64]);
    for (name, offset, size) in &placed {
        for (v, n) in [(*name, 4), (1, 4), (0, 8), (0, 8), (*offset, 8), (*size, 8), (0, 4), (0, 4), (1, 8), (0, 8)] {
            body.extend_from_slice(&v.to_le_bytes()[..n]);
        }
    }
    body[..6].copy_from_slice(b"\x7fELF\x02\x01");
    body[0x28..0x30].copy_from_slice(&shoff.to_le_bytes());
    let count = placed.len() as u16;
    for (at, v) in [(0x3A, 64u16), (0x3C, count + 1), (0x3E, count)] {
        body[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }
    body
}

fn u32s(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn macho(commands: &[&[u8]]) -> Vec<u8> {
    let size: usize = commands.iter().map(|it| it.len()).sum();
    let mut out = u32s(&[0xFEED_FACF, 0, 0, 2, commands.len() as u32, size as u32, 0, 0]);
    for command in commands {
        out.extend_from_slice(command);
    }
    out
}

fn selftest() -> Result<(), Fault> {
    let trace = "thread 'main' panicked at src/main.rs:6:21:
debug side 1
stack backtrace:
   0: rust_begin_unwind
             at /rustc/abc/library/std/src/panicking.rs:695:5
   1: core::panicking::panic_fmt
             at /rustc/abc/library/core/src/panicking.rs:75:14
   2: debugside::deep::h0123456789abcdef
             at ./src/main.rs:6:21
   3:     0x55d0c0de - debugside::main
             at ./src/main.rs:2:5
   4: core::ops::function::FnOnce::call_once
note: Some details are omitted
";
    let own = own_frames(trace);
    let want: Vec<Frame> = vec![
        ("debugside::deep".to_owned(), "./src/main.rs".to_owned(), 6),
        ("debugside::main".to_owned(), "./src/main.rs".to_owned(), 2),
    ];
    expect(SCRIPT, own == want, &format!("the program's frames are read with their places: {}", repr_frames(&own)));
    expect(SCRIPT, frames(trace).len() == 4, "a frame with no place is left out");
    let lines = splitlines(MAIN);
    expect(SCRIPT, lines.get(PANIC_LINE as usize - 1).is_some_and(|it| it.starts_with("fn deep")), "PANIC_LINE is the panic's line");
    let mut note = u32s(&[4, 16, 3]);
    note.extend_from_slice(b"GNU\0");
    note.extend(0u8..16);
    let side = test_elf(&[(".note.gnu.build-id", note.clone()), (".debug_line", b"rows".to_vec())]);
    let mut link = b"debugside.debug\0".to_vec();
    link.extend_from_slice(&crc32(&side).to_le_bytes());
    let exe = test_elf(&[(".note.gnu.build-id", note.clone()), (".gnu_debuglink", link), (".text", vec![0xC3])]);
    expect(SCRIPT, carried(&exe)?.is_empty() && carried(&side)? == [".debug_line"], "debug sections are found by name");
    expect(
        SCRIPT,
        side_of(Path::new("/t/debug/debugside"), &exe)? == Path::new("/t/debug/debugside.debug"),
        "the side file is the one named",
    );
    let same = same_build(&exe, &side)?;
    expect(SCRIPT, same.is_empty(), &format!("a side file of the executable is its own: {same}"));
    let mut other_note = note[..note.len() - 1].to_vec();
    other_note.push(0xFF);
    let other = test_elf(&[(".note.gnu.build-id", other_note), (".debug_line", b"rows".to_vec())]);
    expect(SCRIPT, same_build(&exe, &other)?.contains("build id"), "a side file of another build id is caught");
    let rowz = test_elf(&[(".note.gnu.build-id", note), (".debug_line", b"rowz".to_vec())]);
    expect(SCRIPT, same_build(&exe, &rowz)?.contains("CRC-32"), "a side file of another CRC is caught");
    let mut uuid = u32s(&[0x1B, 24]);
    uuid.extend(0u8..16);
    let mut segment = u32s(&[0x19, 72]);
    let mut name = b"__DWARF".to_vec();
    name.resize(16, 0);
    segment.extend_from_slice(&name);
    segment.extend_from_slice(&[0; 48]);
    expect(SCRIPT, macho_uuid(&macho(&[&uuid, &segment]))? == Some((0u8..16).collect()), "LC_UUID is read");
    expect(
        SCRIPT,
        carried(&macho(&[&uuid, &segment]))? == ["__DWARF"] && carried(&macho(&[&uuid]))?.is_empty(),
        "__DWARF is found",
    );
    expect(SCRIPT, same_build(&macho(&[&uuid]), &macho(&[&uuid, &segment]))?.is_empty(), "a dSYM of the executable's UUID is its own");
    expect(
        SCRIPT,
        side_of(Path::new("/t/debug/app"), &macho(&[&uuid]))? == Path::new("/t/debug/app.dSYM/Contents/Resources/DWARF/app"),
        "the dSYM is beside the executable",
    );
    println!("{SCRIPT} selftest: ok");
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [one] = args.as_slice() else {
        eprintln!("usage: {SCRIPT} '<json>' | selftest\n\n{DOC}");
        blaze_script::exit(1)
    };
    if one == "selftest" {
        if let Err(f) = selftest() {
            fail(SCRIPT, f.said());
        }
    } else {
        main_with(&input::parse::<Args>(one, SCRIPT))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_is_zlibs() {
        // zlib.crc32(b"123456789") and zlib.crc32(b"").
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn a_short_file_is_an_error_not_a_panic() {
        for data in [&b""[..], b"\x7fELF\x02\x01", b"\xcf\xfa\xed\xfe"] {
            assert!(carried(data).is_err(), "{data:?}");
        }
    }

    #[test]
    fn the_selftest_holds() {
        selftest().unwrap();
    }
}
