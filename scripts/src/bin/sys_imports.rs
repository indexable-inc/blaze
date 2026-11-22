//! sys_imports: blaze-sys references no C-library symbol on Linux, and on macOS imports only the
//! libSystem symbols tools/ratchets/blaze-sys-libsystem.txt lists (docs/v2/plan/design/end-state.md,
//! D180; the owner's no-C-library decision of 2026-11-20). `DOC` below is what its usage error
//! prints; it says what is read, what is dropped as Rust's own, and the rule.
//!
//!     blaze run -q -p scripts --bin sys_imports -- '{"command": "check", "target": "linux", "objects": ["<rlib>"], "builtins": "<rlib>", "nm": "<llvm-nm>"}'
//!     blaze run -q -p scripts --bin sys_imports -- '{"command": "check", "target": "macos", "objects": ["<rlib>"], "builtins": "<rlib>", "nm": "<llvm-nm>", "ratchet": "tools/ratchets/blaze-sys-libsystem.txt"}'
//!     blaze run -q -p scripts --bin sys_imports -- '{"command": "write", "target": "macos", ...the same...}'
//!     blaze run -q -p scripts --bin sys_imports -- selftest
//!
//! Why (2026-11-20): on Linux every call of blaze-sys is a raw system call through its own
//! `syscall`/`svc` wrapper (crates/blaze-sys/src/linux/raw.rs), so the crate needs no C library;
//! on macOS libSystem is the floor (Apple keeps no stable system call numbers) and each import is
//! a decision with its reason. A source rule (no `extern "C"` block compiled for Linux) says the
//! same thing from the text; this reads what the compiler made, so a reference no reading of the
//! source finds (a builtin LLVM lowers to a C call, a thread-local's `__tls_get_addr`) is found
//! too. It builds nothing: the caller builds blaze-sys for the target and names the rlib.

/// What the usage error prints.
const DOC: &str = r#"sys_imports: blaze-sys references no C-library symbol on Linux; on macOS it imports only the libSystem symbols its list names (D180, the no-C-library decision of 2026-11-20).

    blaze run -q -p scripts --bin sys_imports -- '{"command": "check", "target": "linux", "objects": [..], "builtins": "..", "nm": ".."}'
    blaze run -q -p scripts --bin sys_imports -- '{"command": "check", "target": "macos", "objects": [..], "builtins": "..", "nm": "..", "ratchet": ".."}'
    blaze run -q -p scripts --bin sys_imports -- '{"command": "write", "target": "macos", "objects": [..], "builtins": "..", "nm": "..", "ratchet": ".."}'
    blaze run -q -p scripts --bin sys_imports -- selftest

    command   check   linux: what is left (below) is empty, or each symbol is named with the
                      object it is in; macos: what is left equals the list: a symbol the list
                      lacks is refused ("a new libSystem import is a decision: add it to the
                      ratchet with its reason"), a row no longer imported is refused ("remove it
                      from the ratchet"), and so is a row whose reason is `(reason needed)`
              write   macos only: the list written from what is left, each kept row's reason
                      kept and a new row's reason `(reason needed)`, which `check` refuses
    target    linux or macos: the target blaze-sys was built for (x86_64 or aarch64 Linux;
              aarch64-apple-darwin)
    objects   the built blaze-sys: its rlib (`target/<triple>/debug/libblaze_sys.rlib`) or its
              object files, one or more
    builtins  the same target's compiler_builtins rlib (with -Zbuild-std,
              `target/<triple>/debug/deps/libcompiler_builtins-<hash>.rlib`; else the sysroot's
              `lib/rustlib/<triple>/lib/libcompiler_builtins-<hash>.rlib`)
    nm        the llvm-nm to run: clang.json's shelf program `llvm-nm` (<shelf>/bin/llvm-nm) on
              a Mac; any llvm-nm reads both ELF and Mach-O on any host
    ratchet   the list's path (tools/ratchets/blaze-sys-libsystem.txt): required for macos,
              refused for linux, which has none

How it reads: `<nm> -A -P -u <file>` lists each object's undefined symbols (an rlib's members
that are not objects, its metadata, are skipped by nm), and `<nm> -A -P -g --defined-only <file>`
the global symbols the objects and the builtins define. On macOS a C name is written with a
leading underscore, which is taken off (`___error` is `__error`); a macOS undefined name without
one, or a Linux one that starts `__ZN`, means the target is not the files', and is refused.

What is dropped, as Rust's own or the compiler's, never a C library's:
  rust       mangled names (`_ZN..`, `_R` and a capital or digit), `rust_*`, `__rust_*`
  builtins   every symbol the builtins rlib defines (the integer and float helpers, aarch64's
             outline atomics, the libm subset), and memcpy, memmove, memset, memcmp, bcmp and
             strlen by name: compiler-builtins' `mem` module defines them
             (library/compiler-builtins/compiler-builtins/src/mem/mod.rs) when its `mem`
             feature is on, as in a program with no C library, and codegen emits them for any
             copy or comparison
  own        a symbol the objects themselves define (one codegen unit's reference to another's
             `#[no_mangle]` item, a `global_asm!` label, macOS's cold table)
  unwind     `_Unwind_*`
  probes     `__chkstk`, `___chkstk_ms`, `__chkstk_darwin` and their kind (the stack probes)
  vdso       on Linux, `__vdso_*` and `__kernel_*`: resolved at run time from the auxiliary
             vector and called through a pointer, so never undefined at link time
What is left is what a C library would have to supply.

What it cannot see: code the rlib does not hold. A generic or `#[inline]` function is compiled
in the crate that uses it, so a C call in one shows in that crate's objects, not here; the
source rule (no `extern "C"` block compiled for Linux) covers those.
"#;

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::Path,
    time::Duration,
};

use blaze_script::{
    fail,
    input::{self, Fields, Input, InputError},
    read_text, run_within,
};

const SCRIPT: &str = "sys_imports";
/// How long one nm may run (rule 69): an rlib of a few megabytes lists in well under a second.
const NM_SECONDS: u64 = 120;
/// The reason `write` gives a new row, which `check` refuses.
const NEEDS_REASON: &str = "(reason needed)";
/// What compiler-builtins' `mem` module defines, by name.
const MEM: [&str; 6] = ["memcpy", "memmove", "memset", "memcmp", "bcmp", "strlen"];
/// The stack probes' names with their leading underscores taken off.
const PROBES: [&str; 4] = ["chkstk", "chkstk_ms", "chkstk_darwin", "probestack"];
/// What the Linux refusal ends with.
const LINUX_RULE: &str = "on Linux blaze-sys references no C-library symbol (D180, 2026-11-20): every call is a raw \
                          system call (crates/blaze-sys/src/linux/raw.rs), so make it one in a file under \
                          crates/blaze-sys/src/linux/, or gate the item to macOS when it has no kernel form";
/// The list's first lines.
const HEADER: &str = "# The libSystem symbols blaze-sys imports on macOS (aarch64-apple-darwin, outside cfg(test)),\n\
                      # one a row, `<name>  # <why, or the area that calls it>`, sorted by the name C gives it (nm\n\
                      # shows each with a leading underscore, not written here). Linux has no list: there blaze-sys\n\
                      # references no C-library symbol at all (D180, the no-C-library decision of 2026-11-20). A\n\
                      # call of the cold table of crates/blaze-sys/src/macos.rs is not here: blaze-sys defines it\n\
                      # and finds libSystem's by `dlsym` at its first call. A new import is a decision: add it with\n\
                      # its reason; a gone one is removed. Checked by scripts/src/bin/sys_imports.rs (`check`,\n\
                      # target macos) against a built rlib; `write` rewrites it, keeping each reason.\n";

/// The target blaze-sys was built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Linux,
    Macos,
}

impl Target {
    const fn name(self) -> &'static str {
        match self {
            Target::Linux => "linux",
            Target::Macos => "macos",
        }
    }
}

/// What the script does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Check,
    Write,
}

/// The one input.
#[derive(Debug, Clone)]
struct Args {
    command: Command,
    target: Target,
    objects: Vec<String>,
    builtins: String,
    nm: String,
    ratchet: Option<String>,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["command", "target", "objects", "builtins", "nm", "ratchet"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        let command = match f.one_of("command", &["check", "write"], None)?.as_str() {
            "write" => Command::Write,
            _ => Command::Check,
        };
        let target = match f.one_of("target", &["linux", "macos"], None)?.as_str() {
            "macos" => Target::Macos,
            _ => Target::Linux,
        };
        let objects = f.strs_required("objects")?;
        if objects.is_empty() {
            return Err(InputError(format!("{} names no file: give the built blaze-sys rlib", f.at("objects"))));
        }
        let ratchet = f.str_or_null("ratchet")?;
        match (target, &ratchet, command) {
            (Target::Linux, Some(_), _) => {
                return Err(InputError(format!(
                    "{} is given for linux, which has no list: its remainder must be empty",
                    f.at("ratchet")
                )));
            }
            (Target::Linux, None, Command::Write) => {
                return Err(InputError("`write` is for macos: linux has no list to write".to_owned()));
            }
            (Target::Macos, None, _) => {
                return Err(InputError(format!("field {} is required for macos", f.at("ratchet"))));
            }
            _ => {}
        }
        Ok(Args { command, target, objects, builtins: f.str("builtins", None)?, nm: f.str("nm", None)?, ratchet })
    }
}

/// Why a run could not reach its verdict (rule 19: each names the file and the program).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ImportsError {
    /// nm could not be started.
    NmNotStarted { nm: String, why: String },
    /// nm ran past [`NM_SECONDS`] and was killed.
    NmTimedOut { nm: String, path: String },
    /// nm exited with a failure.
    NmFailed { nm: String, path: String, status: i32, said: String },
    /// A line of nm's output that is not `<file>: <name> <type> ..`.
    NmLine { path: String, line: usize, text: String },
    /// The objects define no global symbol: they are not a built blaze-sys.
    NothingDefined { paths: String },
    /// A symbol whose form is not the target's: the files were built for another.
    TargetMismatch { target: Target, name: String, member: String },
    /// The list is not there or not UTF-8.
    RatchetUnread { path: String },
    /// A row of the list that is not `<name>  # <reason>`.
    RatchetRow { path: String, line: usize, what: &'static str },
    /// A row out of order, or a name twice.
    RatchetOrder { path: String, line: usize, name: String },
    /// The list could not be written.
    NotWritten { path: String, why: String },
}

impl fmt::Display for ImportsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImportsError::NmNotStarted { nm, why } => write!(f, "{nm} did not start: {why}"),
            ImportsError::NmTimedOut { nm, path } => {
                write!(f, "{nm} on {path} ran past {NM_SECONDS} s and was killed (rule 69: a hang is a finding)")
            }
            ImportsError::NmFailed { nm, path, status, said } => write!(f, "{nm} on {path} exited {status}: {said}"),
            ImportsError::NmLine { path, line, text } => {
                write!(f, "nm's output for {path}, line {line}, is not `<file>: <name> <type>`: {text:?}")
            }
            ImportsError::NothingDefined { paths } => {
                write!(f, "{paths} define no global symbol: not a built blaze-sys (no proof the check read code)")
            }
            ImportsError::TargetMismatch { target, name, member } => write!(
                f,
                "{name:?} in {member} is not a {} object's symbol: the files were built for another target",
                target.name()
            ),
            ImportsError::RatchetUnread { path } => write!(f, "{path} is not there or is not UTF-8"),
            ImportsError::RatchetRow { path, line, what } => write!(f, "{path}:{line}: {what}"),
            ImportsError::RatchetOrder { path, line, name } => {
                write!(f, "{path}:{line}: {name} is out of order or listed twice: the rows are sorted, each name once")
            }
            ImportsError::NotWritten { path, why } => write!(f, "{path} is not written: {why}"),
        }
    }
}

impl std::error::Error for ImportsError {}

/// One symbol of nm's output: the file (and member) it is in, and its name as nm shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sym {
    member: String,
    name: String,
}

/// The symbols of `-A -P` output: each line `<file>: <name> <type> [<value> <size>]`, where
/// `<file>` is `<archive>[<member>]` for an archive's member.
fn parse_nm(path: &str, text: &str) -> Result<Vec<Sym>, ImportsError> {
    let mut out = Vec::new();
    for (at, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let refused = || ImportsError::NmLine { path: path.to_owned(), line: at + 1, text: line.to_owned() };
        let (member, rest) = line.rsplit_once(": ").ok_or_else(refused)?;
        let mut words = rest.split_whitespace();
        let (Some(name), Some(kind)) = (words.next(), words.next()) else { return Err(refused()) };
        if kind.chars().count() != 1 {
            return Err(refused());
        }
        out.push(Sym { member: member.to_owned(), name: name.to_owned() });
    }
    Ok(out)
}

/// The member part of nm's file prefix: `x.o` of `lib.rlib[x.o]`, else the prefix.
fn member_of(prefix: &str) -> &str {
    match prefix.rsplit_once('[') {
        Some((_, rest)) => rest.strip_suffix(']').unwrap_or(rest),
        None => prefix,
    }
}

/// The name as C spells it: macOS's leading underscore taken off.
fn c_name(target: Target, raw: &str) -> &str {
    match target {
        Target::Macos => raw.strip_prefix('_').unwrap_or(raw),
        Target::Linux => raw,
    }
}

/// Refuses an undefined name whose form is not the target's.
fn check_form(target: Target, sym: &Sym) -> Result<(), ImportsError> {
    let foreign = match target {
        Target::Macos => !sym.name.starts_with('_'),
        Target::Linux => sym.name.starts_with("__ZN"),
    };
    if foreign {
        return Err(ImportsError::TargetMismatch { target, name: sym.name.clone(), member: member_of(&sym.member).to_owned() });
    }
    Ok(())
}

/// Why a C name is Rust's own or the compiler's, never a C library's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Runtime {
    Rust,
    Mem,
    Unwind,
    Probe,
    Vdso,
}

fn runtime(target: Target, name: &str) -> Option<Runtime> {
    let v0 = name.strip_prefix("_R").and_then(|rest| rest.bytes().next()).is_some_and(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
    if name.starts_with("_ZN") || v0 || name.starts_with("rust_") || name.starts_with("__rust_") {
        Some(Runtime::Rust)
    } else if MEM.contains(&name) {
        Some(Runtime::Mem)
    } else if name.starts_with("_Unwind_") {
        Some(Runtime::Unwind)
    } else if PROBES.contains(&name.trim_start_matches('_')) {
        Some(Runtime::Probe)
    } else if target == Target::Linux && (name.starts_with("__vdso_") || name.starts_with("__kernel_")) {
        Some(Runtime::Vdso)
    } else {
        None
    }
}

/// What is left: each undefined C name that is not Rust's, the compiler's, the builtins' or the
/// objects' own (`defined`, C names), with the members that reference it.
fn remainder(target: Target, undefined: &[Sym], defined: &BTreeSet<String>) -> Result<BTreeMap<String, BTreeSet<String>>, ImportsError> {
    let mut left: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for sym in undefined {
        check_form(target, sym)?;
        let name = c_name(target, &sym.name);
        if runtime(target, name).is_some() || defined.contains(name) {
            continue;
        }
        left.entry(name.to_owned()).or_default().insert(member_of(&sym.member).to_owned());
    }
    Ok(left)
}

/// `<nm> -A -P` and `flags` on `path`: its symbols.
fn listed(nm: &str, path: &str, flags: &[&str]) -> Result<Vec<Sym>, ImportsError> {
    let mut argv = vec![nm, "-A", "-P"];
    argv.extend_from_slice(flags);
    argv.push(path);
    let ran = run_within(&argv, Path::new("."), Duration::from_secs(NM_SECONDS))
        .map_err(|why| ImportsError::NmNotStarted { nm: nm.to_owned(), why })?
        .ok_or_else(|| ImportsError::NmTimedOut { nm: nm.to_owned(), path: path.to_owned() })?;
    if ran.status != 0 {
        return Err(ImportsError::NmFailed { nm: nm.to_owned(), path: path.to_owned(), status: ran.status, said: ran.err().trim().to_owned() });
    }
    parse_nm(path, &ran.out())
}

/// A row of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    name: String,
    reason: String,
    line: usize,
}

/// The list's rows: `<name>  # <reason>`, sorted, each name once; `#` lines and blank lines are
/// comments.
fn parse_ratchet(path: &str, text: &str) -> Result<Vec<Row>, ImportsError> {
    let mut rows: Vec<Row> = Vec::new();
    for (at, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let line = at + 1;
        let row = |what| ImportsError::RatchetRow { path: path.to_owned(), line, what };
        let (name, reason) = trimmed.split_once('#').ok_or_else(|| row("a row is `<name>  # <reason>`: the reason is missing"))?;
        let (name, reason) = (name.trim(), reason.trim());
        if name.is_empty() || name.contains(char::is_whitespace) {
            return Err(row("a row's name is one word before its `#`"));
        }
        if reason.is_empty() {
            return Err(row("a row's reason after its `#` is empty: a libSystem import is a decision, and says why"));
        }
        if rows.last().is_some_and(|last| last.name.as_str() >= name) {
            return Err(ImportsError::RatchetOrder { path: path.to_owned(), line, name: name.to_owned() });
        }
        rows.push(Row { name: name.to_owned(), reason: reason.to_owned(), line });
    }
    Ok(rows)
}

fn members(of: &BTreeSet<String>) -> String {
    of.iter().map(String::as_str).collect::<Vec<_>>().join(", ")
}

/// Linux: each symbol left, named, with what to do.
fn linux_problems(left: &BTreeMap<String, BTreeSet<String>>) -> Vec<String> {
    left.iter()
        .map(|(name, of)| {
            let hint = match name.as_str() {
                "__tls_get_addr" => " (a thread-local: blaze-sys holds none)",
                "__errno_location" => " (errno: on Linux a raw call returns -errno in its result)",
                _ => "",
            };
            format!("{name}{hint} (in {}): {LINUX_RULE}", members(of))
        })
        .collect()
}

/// macOS: the symbols left against the list.
fn macos_problems(left: &BTreeMap<String, BTreeSet<String>>, rows: &[Row], ratchet: &str) -> Vec<String> {
    let listed: BTreeSet<&str> = rows.iter().map(|row| row.name.as_str()).collect();
    let mut said = Vec::new();
    for (name, of) in left {
        if !listed.contains(name.as_str()) {
            said.push(format!(
                "{name} (in {}) is imported and not in {ratchet}: a new libSystem import is a decision: add it to the ratchet with its reason",
                members(of)
            ));
        }
    }
    for row in rows {
        if !left.contains_key(&row.name) {
            said.push(format!("{} ({ratchet}:{}) is no longer imported: remove it from the ratchet", row.name, row.line));
        } else if row.reason == NEEDS_REASON {
            said.push(format!(
                "{} ({ratchet}:{}) says {NEEDS_REASON}: a new libSystem import is a decision: write its reason",
                row.name, row.line
            ));
        }
    }
    said
}

/// The list as `write` writes it: [`HEADER`], then each symbol left with its kept reason.
fn render(left: &BTreeMap<String, BTreeSet<String>>, rows: &[Row]) -> String {
    let reasons: BTreeMap<&str, &str> = rows.iter().map(|row| (row.name.as_str(), row.reason.as_str())).collect();
    let mut text = HEADER.to_owned();
    for name in left.keys() {
        let reason = reasons.get(name.as_str()).copied().unwrap_or(NEEDS_REASON);
        text.push_str(&format!("{name}  # {reason}\n"));
    }
    text
}

/// The verdict of one run: its problems (empty passes) and the line that says what was read.
fn run(it: &Args) -> Result<(Vec<String>, String), ImportsError> {
    let mut undefined = Vec::new();
    let mut defined = BTreeSet::new();
    for path in &it.objects {
        undefined.extend(listed(&it.nm, path, &["-u"])?);
        for sym in listed(&it.nm, path, &["-g", "--defined-only"])? {
            defined.insert(c_name(it.target, &sym.name).to_owned());
        }
    }
    if defined.is_empty() {
        return Err(ImportsError::NothingDefined { paths: it.objects.join(", ") });
    }
    let own = defined.len();
    for sym in listed(&it.nm, &it.builtins, &["-g", "--defined-only"])? {
        defined.insert(c_name(it.target, &sym.name).to_owned());
    }
    let left = remainder(it.target, &undefined, &defined)?;
    let summary = format!(
        "{SCRIPT}: {}: {} undefined references in {}, {own} names the objects define, {} left",
        it.target.name(),
        undefined.len(),
        it.objects.join(", "),
        left.len()
    );
    let Some(ratchet) = &it.ratchet else { return Ok((linux_problems(&left), summary)) };
    let text = match (read_text(Path::new(ratchet)), it.command) {
        (Some(text), _) => text,
        (None, Command::Write) => String::new(),
        (None, Command::Check) => return Err(ImportsError::RatchetUnread { path: ratchet.clone() }),
    };
    let rows = parse_ratchet(ratchet, &text)?;
    if it.command == Command::Write {
        std::fs::write(ratchet, render(&left, &rows)).map_err(|e| ImportsError::NotWritten { path: ratchet.clone(), why: e.to_string() })?;
        return Ok((Vec::new(), format!("{summary}; {ratchet} written ({} rows)", left.len())));
    }
    Ok((macos_problems(&left, &rows, ratchet), summary))
}

fn main_with(it: &Args) {
    match run(it) {
        Err(e) => fail(SCRIPT, &e.to_string()),
        Ok((problems, summary)) => {
            println!("{summary}");
            if problems.is_empty() {
                println!("{SCRIPT}: ok");
                return;
            }
            for problem in &problems {
                eprintln!("{SCRIPT}: {problem}");
            }
            fail(SCRIPT, &format!("{} symbols refused", problems.len()));
        }
    }
}

fn expect(holds: bool, what: &str) {
    blaze_script::expect(SCRIPT, holds, what);
}

/// A Linux rlib's undefined symbols: Rust's, the builtins', the unwinder's, the vDSO's, one the
/// crate defines itself, and two a C library would supply (`getpid`, `__tls_get_addr`).
const LINUX_UNDEFINED: &str = "libblaze_sys.rlib[blaze_sys-0.cgu.0.rcgu.o]: _ZN4core9panicking9panic_fmt17h0123456789abcdefE U
libblaze_sys.rlib[blaze_sys-0.cgu.0.rcgu.o]: _RNvNtCs1a_4core9panicking9panic_fmt U
libblaze_sys.rlib[blaze_sys-0.cgu.0.rcgu.o]: rust_begin_unwind U
libblaze_sys.rlib[blaze_sys-0.cgu.0.rcgu.o]: __rust_alloc U
libblaze_sys.rlib[blaze_sys-0.cgu.0.rcgu.o]: memcpy U
libblaze_sys.rlib[blaze_sys-0.cgu.0.rcgu.o]: __udivti3 U
libblaze_sys.rlib[blaze_sys-0.cgu.0.rcgu.o]: _Unwind_Resume U
libblaze_sys.rlib[blaze_sys-0.cgu.0.rcgu.o]: __vdso_clock_gettime U
libblaze_sys.rlib[blaze_sys-0.cgu.0.rcgu.o]: blaze_sys_sample_restorer U
libblaze_sys.rlib[blaze_sys-0.cgu.1.rcgu.o]: getpid U
libblaze_sys.rlib[blaze_sys-0.cgu.1.rcgu.o]: __tls_get_addr w
";
/// What the Linux rlib defines (a `global_asm!` label) and what its builtins define.
const LINUX_DEFINED: &str = "libblaze_sys.rlib[blaze_sys-0.cgu.1.rcgu.o]: blaze_sys_sample_restorer T 0 10\n";
const LINUX_BUILTINS: &str = "libcompiler_builtins.rlib[compiler_builtins-0.o]: __udivti3 T 0 40\n";

/// A macOS rlib's: Rust's, `memcpy`, one the cold table defines (`pread`), and two of libSystem.
const MACOS_UNDEFINED: &str = "libblaze_sys.rlib[a.o]: __ZN4core9panicking9panic_fmt17h0123456789abcdefE U
libblaze_sys.rlib[a.o]: _dlsym U
libblaze_sys.rlib[a.o]: ___error U
libblaze_sys.rlib[a.o]: _memcpy U
libblaze_sys.rlib[a.o]: _pread U
";
const MACOS_DEFINED: &str = "libblaze_sys.rlib[b.o]: _pread T 0 20\n";

fn defined_of(target: Target, texts: &[&str]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for text in texts {
        let syms = parse_nm("fixture", text).unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e.to_string()));
        out.extend(syms.iter().map(|sym| c_name(target, &sym.name).to_owned()));
    }
    out
}

fn left_of(target: Target, undefined: &str, defined: &[&str]) -> Result<BTreeMap<String, BTreeSet<String>>, ImportsError> {
    let syms = parse_nm("fixture", undefined)?;
    remainder(target, &syms, &defined_of(target, defined))
}

fn names(left: &BTreeMap<String, BTreeSet<String>>) -> Vec<&str> {
    left.keys().map(String::as_str).collect()
}

fn selftest() {
    // nm's lines.
    let syms = parse_nm("x", "lib.rlib[m.o]: name U\n\nlib.rlib[m.o]: other T 0 10\n");
    expect(
        syms.as_ref().is_ok_and(|it| it.len() == 2 && it.first().is_some_and(|s| s.name == "name" && member_of(&s.member) == "m.o")),
        &format!("two lines read, the blank one skipped: {syms:?}"),
    );
    expect(matches!(parse_nm("x", "garbage\n"), Err(ImportsError::NmLine { line: 1, .. })), "a line with no `: ` is refused");
    expect(matches!(parse_nm("x", "f.o: name\n"), Err(ImportsError::NmLine { .. })), "a line with no type is refused");

    // Linux: Rust's, the builtins', the unwinder's, the vDSO's and the crate's own are dropped.
    let linux = left_of(Target::Linux, LINUX_UNDEFINED, &[LINUX_DEFINED, LINUX_BUILTINS]);
    let got = linux.as_ref().map(names);
    expect(got.as_ref().is_ok_and(|it| *it == ["__tls_get_addr", "getpid"]), &format!("what is left on Linux: {got:?}"));
    let said = linux.as_ref().map(linux_problems).unwrap_or_default();
    expect(
        said.len() == 2 && said.iter().any(|it| it.starts_with("getpid (in blaze_sys-0.cgu.1.rcgu.o)") && it.contains("raw system call")),
        &format!("the Linux refusal names each symbol and its object: {said:?}"),
    );
    // Planted: each cause removed, its effect goes.
    let clean = LINUX_UNDEFINED.lines().filter(|it| !it.contains("getpid") && !it.contains("__tls_get_addr")).collect::<Vec<_>>().join("\n");
    let left = left_of(Target::Linux, &clean, &[LINUX_DEFINED, LINUX_BUILTINS]);
    expect(left.as_ref().is_ok_and(BTreeMap::is_empty), &format!("without getpid and __tls_get_addr nothing is left: {left:?}"));
    let left = left_of(Target::Linux, &clean, &[LINUX_DEFINED]);
    expect(left.as_ref().is_ok_and(|it| names(it) == ["__udivti3"]), &format!("without the builtins' definition __udivti3 is left: {left:?}"));
    let left = left_of(Target::Linux, &clean, &[LINUX_BUILTINS]);
    expect(
        left.as_ref().is_ok_and(|it| names(it) == ["blaze_sys_sample_restorer"]),
        &format!("without the crate's own definition its label is left: {left:?}"),
    );
    expect(
        matches!(left_of(Target::Linux, MACOS_UNDEFINED, &[]), Err(ImportsError::TargetMismatch { .. })),
        "a macOS rlib checked as Linux is refused",
    );
    expect(
        matches!(left_of(Target::Macos, LINUX_UNDEFINED, &[]), Err(ImportsError::TargetMismatch { .. })),
        "a Linux rlib checked as macOS is refused",
    );

    // macOS: the underscore off, the cold table's own definition dropped, the list compared.
    let macos = left_of(Target::Macos, MACOS_UNDEFINED, &[MACOS_DEFINED]).unwrap_or_default();
    expect(names(&macos) == ["__error", "dlsym"], &format!("what is left on macOS: {macos:?}"));
    let list = "# a comment\n__error  # errno\ndlsym  # the cold table's binding\n";
    let rows = parse_ratchet("l", list).unwrap_or_default();
    expect(rows.len() == 2 && macos_problems(&macos, &rows, "l").is_empty(), &format!("the list as imported: {rows:?}"));
    let rows = parse_ratchet("l", "__error  # errno\n").unwrap_or_default();
    let said = macos_problems(&macos, &rows, "l");
    expect(
        said.len() == 1 && said.iter().all(|it| it.starts_with("dlsym") && it.contains("add it to the ratchet with its reason")),
        &format!("a new import: {said:?}"),
    );
    let rows = parse_ratchet("l", "__error  # errno\ndlsym  # binding\nkill  # signals\n").unwrap_or_default();
    let said = macos_problems(&macos, &rows, "l");
    expect(
        said.len() == 1 && said.iter().all(|it| it.starts_with("kill (l:3)") && it.contains("remove it from the ratchet")),
        &format!("a gone import: {said:?}"),
    );
    expect(matches!(parse_ratchet("l", "dlsym  # a\n__error  # b\n"), Err(ImportsError::RatchetOrder { line: 2, .. })), "unsorted rows are refused");
    expect(matches!(parse_ratchet("l", "dlsym  # a\ndlsym  # b\n"), Err(ImportsError::RatchetOrder { line: 2, .. })), "a name twice is refused");
    expect(matches!(parse_ratchet("l", "dlsym\n"), Err(ImportsError::RatchetRow { line: 1, .. })), "a row with no reason is refused");
    expect(matches!(parse_ratchet("l", "dlsym  #  \n"), Err(ImportsError::RatchetRow { line: 1, .. })), "an empty reason is refused");

    // write: the kept reason stays, a new row says it needs one, and check refuses that.
    let written = render(&macos, &parse_ratchet("l", "__error  # errno\n").unwrap_or_default());
    let rows = parse_ratchet("l", &written).unwrap_or_default();
    expect(
        rows.iter().map(|row| (row.name.as_str(), row.reason.as_str())).eq([("__error", "errno"), ("dlsym", NEEDS_REASON)]),
        &format!("the written list: {written:?}"),
    );
    let said = macos_problems(&macos, &rows, "l");
    expect(said.len() == 1 && said.iter().all(|it| it.starts_with("dlsym") && it.contains("write its reason")), &format!("a reason needed: {said:?}"));

    // The classes, one name each.
    let cases = [
        (Target::Linux, "_ZN4core3fmt5write17h0000000000000000E", Some(Runtime::Rust)),
        (Target::Linux, "_RNvCs1_4core4main", Some(Runtime::Rust)),
        (Target::Linux, "__rust_probestack", Some(Runtime::Rust)),
        (Target::Linux, "strlen", Some(Runtime::Mem)),
        (Target::Linux, "_Unwind_Resume", Some(Runtime::Unwind)),
        (Target::Linux, "___chkstk_ms", Some(Runtime::Probe)),
        (Target::Macos, "__chkstk_darwin", Some(Runtime::Probe)),
        (Target::Linux, "__kernel_clock_gettime", Some(Runtime::Vdso)),
        (Target::Macos, "__kernel_clock_gettime", None),
        (Target::Linux, "_Rmy_c_name", None),
        (Target::Linux, "__errno_location", None),
    ];
    for (target, name, want) in cases {
        expect(runtime(target, name) == want, &format!("{name} on {}: {:?}, not {want:?}", target.name(), runtime(target, name)));
    }
    println!("{SCRIPT} selftest: ok");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [one] if one == "selftest" => selftest(),
        [one] => main_with(&input::parse::<Args>(one, SCRIPT)),
        _ => {
            eprintln!("usage: {SCRIPT} '<json>' | selftest\n\n{DOC}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The script's own cases run under `blaze test` and `cargo test` too.
    #[test]
    fn the_selftest_holds() {
        selftest();
    }
}
