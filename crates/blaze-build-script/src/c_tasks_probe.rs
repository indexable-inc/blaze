//! The compiler probes a wasm build script makes, answered from the pinned table
//! pins/answers/clang.json (c.md D174, design A; rules 1, 3, 21, 25), sans-IO.
//!
//! A script that builds C with the `cc` crate asks its compiler questions before it compiles
//! anything: which family it is (`cc -E <OUT_DIR>/<n>detect_compiler_family.c`, whose stdout
//! names `clang`), whether it is MSVC (`cc -?`) and, for each `flag_if_supported`, whether a
//! flag compiles cleanly (`-o <OUT_DIR>/flag_check -c <OUT_DIR>/flag_check.c`, supported when
//! the exit is 0 and stderr empty). A check runs no compiler, and a deferred request answered
//! exit 0 is a lie the script acts on (every flag supported, `-?` taken for MSVC). So each probe
//! is answered from the table, which holds what the shelf's clang printed for it
//! (scripts/src/bin/clang_answers.rs runs the clang the table names and writes the answers).
//!
//! A row is keyed by what is asked, all of it (rule 21): the clang archive's sha256 (the
//! shelf's name), the program's name, the arguments, the spawn's variables clang reads
//! ([`CLANG_ENV`]) and the content of each file the probe reads. Where the script put a probe's
//! files and what it named them is not asked: cc's `NamedTempfile` puts per-run digits before a
//! name (`563577319973579455detect_compiler_family.c`), and a script may give a build its own
//! out folder below `OUT_DIR` (aws-lc-sys's `out-<check>/`). So an argument that is a path
//! under the guest `OUT_DIR`, at any depth, is spelled by a placeholder: [`INPUT`]`<n><ext>` when
//! a file is there (its sha256 in the row's `files`), [`OUTPUT`]`<n><ext>` when none is (what the
//! probe writes), `n` counting each kind in argument order and `<ext>` the name's extension (clang
//! takes the language from it). Any other `OUT_DIR` or SDK folder in an argument is spelled
//! `$OUT_DIR` or `$SDKROOT`. Each placeholder is put back as the script's path in the answer. A
//! probe with no row, or a row still PENDING, is a [`ProbeRefused`] that prints the row to add
//! (rule 3: never an answer made up).
//!
//! The table is in every script's base key ([`table_id`], `key::base`), so a changed answer
//! re-runs the scripts and no read is recorded per probe.

#![blaze::sans_io]

use core::fmt;

use blaze_ids::{ContentId, Hasher};
use blaze_json::{Event, Limits, Reader, Str};
use blaze_own::{AString, AVec, Alloc, String, Vec};

/// The table's path in a checkout (blaze-pins includes it).
pub const PATH: &str = "pins/answers/clang.json";
/// The one form of the table read here (its `form`).
pub const FORM: u64 = 1;
const ID_DOMAIN: &str = "blaze-build-script clang answers table 1";
/// Domain for canonical [`ProbeKey`] hashing (answerer id, clang sha256, sdk/sysroot id, program,
/// canonical args, canonical `CLANG_ENV`, sorted input file sha256s).
pub const PROBE_KEY_DOMAIN: &str = "blaze-build-script c probe 1";
/// Domain for [`canonical_reply_digest`] (matches `spawn_route::reply_digest` so `fact_now` and
/// `spawn_probe` agree).
const REPLY_DOMAIN: &str = "blaze build-script spawn reply 1";
/// Magic bytes of an encoded binary probe cache record (`bzcp` v1).
const ANSWER_MAGIC: [u8; 4] = *b"bzcp";
const ANSWER_VERSION: u32 = 1;

/// The guest `OUT_DIR`'s spelling in a row.
pub const OUT_DIR: &str = "$OUT_DIR";
/// The guest SDK folder's spelling in a row.
pub const SDKROOT: &str = "$SDKROOT";
/// The script's root mount / package directory spelling in a row (`/package`, `/workspace/...`).
pub const MOUNT: &str = "$MOUNT";
/// The head of a file a probe reads under `OUT_DIR`, as a row spells it (`$IN0.c`).
pub const INPUT: &str = "$IN";
/// The head of a path under `OUT_DIR` a probe names where no file is (`-o $OUT0`).
pub const OUTPUT: &str = "$OUT";

/// The environment variables clang's driver reads that can change a probe's answer, sorted. A
/// spawn's value of each is part of its row; every other variable is not read by clang.
/// `TMPDIR` is intentionally excluded: host ambient temp dirs (e.g. `/private/var/folders/...`)
/// must never pollute content-addressed probe keys; hermetic execution supplies the probe's own
/// private confined output directory as `TMPDIR`.
pub const CLANG_ENV: [&str; 10] = [
    "CCC_OVERRIDE_OPTIONS",
    "COMPILER_PATH",
    "CPATH",
    "CPLUS_INCLUDE_PATH",
    "C_INCLUDE_PATH",
    "MACOSX_DEPLOYMENT_TARGET",
    "OBJCPLUS_INCLUDE_PATH",
    "OBJC_INCLUDE_PATH",
    "SDKROOT",
    "SOURCE_DATE_EPOCH",
];

/// The C and C++ compiler names a probe is spawned as.
const COMPILERS: &[&str] = &["c++", "cc", "clang", "clang++", "g++", "gcc"];

fn name_of(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

/// Normalizes a path under `out_dir`, collapsing `.` and repeated `/` separators and rejecting
/// `..` traversal or trailing directory slashes, so `-o /out/./flag_check /out/./flag_check.c`
/// resolves to the exact same `OUT_DIR`-relative key (`flag_check.c`) as the sandbox filesystem.
pub fn normalize_under_out_dir<'a>(path: &str, out_dir: &str, alloc: Alloc<'a>) -> Option<AString<'a>> {
    let trimmed_out = out_dir.trim_end_matches('/');
    if trimmed_out.is_empty() {
        return None;
    }
    let rel = path.strip_prefix(trimmed_out)?.strip_prefix('/')?;
    if rel.is_empty() || rel.ends_with('/') {
        return None;
    }
    let mut normalized = String::new_in(alloc);
    for seg in rel.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            return None;
        }
        if !normalized.is_empty() {
            normalized.push('/');
        }
        normalized.push_str(seg);
    }
    if normalized.is_empty() {
        return None;
    }
    Some(normalized)
}

/// Whether `path` lies under `out_dir` after collapsing `.` and repeated `/` separators (with no
/// `..` traversal or trailing slash).
fn is_under_out_dir(path: &str, out_dir: &str) -> bool {
    let trimmed_out = out_dir.trim_end_matches('/');
    if trimmed_out.is_empty() {
        return false;
    }
    let Some(rel) = path.strip_prefix(trimmed_out).and_then(|r| r.strip_prefix('/')) else {
        return false;
    };
    if rel.is_empty() || rel.ends_with('/') {
        return false;
    }
    let mut saw_part = false;
    for seg in rel.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            return false;
        }
        saw_part = true;
    }
    saw_part
}

/// Whether `src` is the exact `<dir>/flag_check.<c-ext>` companion to `out_path` (`<dir>/flag_check`)
/// after collapsing `.` and repeated `/` separators in both paths.
fn is_flag_check_companion(out_path: &str, src: &str) -> bool {
    let Some(base) = src
        .strip_suffix(".c")
        .or_else(|| src.strip_suffix(".cpp"))
        .or_else(|| src.strip_suffix(".cc"))
        .or_else(|| src.strip_suffix(".cxx"))
        .or_else(|| src.strip_suffix(".m"))
        .or_else(|| src.strip_suffix(".mm"))
    else {
        return false;
    };
    if out_path.starts_with('/') != base.starts_with('/') {
        return false;
    }
    let mut a = out_path.split('/').filter(|s| !s.is_empty() && *s != ".");
    let mut b = base.split('/').filter(|s| !s.is_empty() && *s != ".");
    loop {
        match (a.next(), b.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) if x == y && x != ".." => {}
            _ => return false,
        }
    }
}

/// Implementation identity of compiler-probe classification, canonicalization, and input
/// filtering (AGENTS.md rule 69: derived from source bytes, never a hand-bumped version).
pub fn implementation_id() -> ContentId {
    ContentId::of("blaze-build-script c_tasks_probe implementation", include_bytes!("c_tasks_probe.rs"))
}

fn is_header_search_flag(arg: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "-I",
        "-isystem",
        "-isysroot",
        "-iquote",
        "-idirafter",
        "-iprefix",
        "-iwithprefix",
        "-iwithprefixbefore",
        "-include",
        "-imacros",
        "-iframework",
        "-F",
        "--include",
        "--include-directory",
        "-Wp,",
        "-Xpreprocessor",
        "-Xclang",
        "-fmodule-file",
        "-fmodule-map-file",
        "-fprebuilt-module-path",
        "-fmodules-cache-path",
        "-fembed-dir",
        "-fsanitize-ignorelist",
        "-fsanitize-blacklist",
        "-fsanitize-coverage-allowlist",
        "-fsanitize-coverage-ignorelist",
        "-fprofile-list",
        "-fxray-attr-list",
        "-fxray-always-instrument",
        "-fxray-never-instrument",
        "-foptimization-record-file",
        "-fdepfile-entry",
        "-fpass-plugin",
        "-fplugin",
        "--config",
        "-resource-dir",
        "-working-directory",
        "--working-directory",
        "-dumpdir",
        "-save-temps",
        "-B",
        "--gcc-toolchain",
        "--sysroot",
        "-ivfsoverlay",
    ];
    if PREFIXES.iter().any(|p| arg.starts_with(p)) {
        return true;
    }
    // Closed-world guard: refuse any `flag=value` option whose value contains a path separator
    // (other than safe string-rewriting prefix-map / compilation-dir metadata flags and `-D` macro
    // definitions) or whose name explicitly references an external file/list/dir/directory/path/
    // sysroot/plugin/overlay.
    if !is_prefix_map_flag(arg)
        && !is_metadata_dir_flag(arg)
        && !arg.starts_with("-D")
        && let Some((name, val)) = arg.split_once('=')
    {
        if val.contains('/') || val.contains('\\') {
            return true;
        }
        for suffix in [
            "-file",
            "-list",
            "-dir",
            "-directory",
            "directory",
            "-path",
            "-output",
            "-input",
            "-map",
            "sysroot",
            "include",
            "embed",
            "plugin",
            "config",
            "allowlist",
            "ignorelist",
            "blacklist",
            "overlay",
        ] {
            if name.ends_with(suffix) {
                return true;
            }
        }
    }
    false
}

/// Whether `arg` causes the compiler to write additional output files (depfiles, serialized
/// diagnostics, temporary intermediates, trace/stats/optimization records, module outputs, or
/// coverage notes) that a probe runner does not return to the build script.
fn is_extra_output_flag(arg: &str) -> bool {
    matches!(
        arg,
        "-M" | "-MM"
            | "-MD"
            | "-MMD"
            | "-MG"
            | "-MP"
            | "-MV"
            | "-H"
            | "--write-dependencies"
            | "--write-user-dependencies"
            | "-dependency-file"
            | "--dependency-file"
            | "-serialize-diagnostics"
            | "--serialize-diagnostics"
            | "-save-temps"
            | "--save-temps"
            | "-save-stats"
            | "--save-stats"
            | "-gen-reproducer"
            | "-mllvm"
            | "-dumpdir"
            | "-dumpbase"
            | "-dumpbase-ext"
            | "-auxbase"
            | "-auxbase-strip"
            | "-fsave-optimization-record"
            | "-foptimization-record-file"
            | "-fdeps-file"
            | "-fmodule-output"
            | "-fmodules-cache-path"
            | "-fprebuilt-module-path"
            | "-fcrash-diagnostics-dir"
            | "-ftime-trace"
            | "-fproc-stat-report"
            | "-ftest-coverage"
            | "--coverage"
            | "-gsplit-dwarf"
            | "--split-dwarf"
            | "-split-dwarf-file"
            | "-split-dwarf-output"
            | "-fno-integrated-as"
            | "-no-integrated-as"
            | "-fno-integrated-cc1"
            | "-no-integrated-cc1"
            | "-fno-integrated-cpp"
            | "-no-integrated-cpp"
            | "-fuse-ld"
            | "--output"
            | "-Xpreprocessor"
            | "-Xclang"
            | "-Xassembler"
            | "-Xlinker"
    ) || arg.starts_with("-MF")
        || arg.starts_with("-MQ")
        || arg.starts_with("-MT")
        || arg.starts_with("-MJ")
        || arg.starts_with("-Wp,")
        || arg.starts_with("-Wa,")
        || arg.starts_with("-Wl,")
        || arg.starts_with("-gsplit-dwarf=")
        || arg.starts_with("--split-dwarf=")
        || arg.starts_with("-split-dwarf-file=")
        || arg.starts_with("-split-dwarf-output=")
        || arg.starts_with("-fuse-ld=")
        || arg.starts_with("-dependency-file=")
        || arg.starts_with("--dependency-file=")
        || arg.starts_with("-serialize-diagnostics=")
        || arg.starts_with("--serialize-diagnostics=")
        || arg.starts_with("-save-temps=")
        || arg.starts_with("--save-temps=")
        || arg.starts_with("-save-stats=")
        || arg.starts_with("--save-stats=")
        || arg.starts_with("-gen-reproducer=")
        || arg.starts_with("-fdeps-file=")
        || arg.starts_with("-fmodule-output=")
        || arg.starts_with("-fmodules-cache-path=")
        || arg.starts_with("-fprebuilt-module-path=")
        || arg.starts_with("-fcrash-diagnostics-dir=")
        || arg.starts_with("-ftime-trace=")
        || arg.starts_with("-fproc-stat-report=")
        || arg.starts_with("-fsave-optimization-record=")
        || arg.starts_with("-foptimization-record-file=")
        || arg.starts_with("--output=")
}

/// Matches the exact `cc-rs` `is_flag_supported` probe shape: compile-only (`-c`), with
/// **exactly one** `-o <dir>/flag_check` output option (no duplicate `-o`, attached `-o...`, or
/// extra side-effect output flags) and ONLY the companion `<dir>/flag_check.<c-ext>` source input
/// in the same directory so custom build scripts that happen to name a real object/binary
/// `flag_check` stay on the normal compiler route and preserve their output files.
fn is_flag_check_probe(args: &[&str]) -> bool {
    if !args.contains(&"-c") {
        return false;
    }
    let mut out_path: Option<&str> = None;
    let mut saw_flag_src = false;
    let mut prev_flag: Option<&str> = None;
    for &arg in args {
        if let Some(prev) = prev_flag.take() {
            if prev == "-o" {
                if out_path.is_some() || name_of(arg) != "flag_check" {
                    return false;
                }
                out_path = Some(arg);
            }
            continue;
        }
        if (arg.starts_with("-o") && arg.len() > 2) || is_extra_output_flag(arg) {
            // Attached `-o<path>` or side-effect output flag: not a discardable flag_check probe.
            return false;
        }
        if flag_takes_value(arg) {
            prev_flag = Some(arg);
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        let Some(out) = out_path else {
            // Check after loop if source preceded `-o`: only record once we know `out_path`.
            // Actually collect in a second pass below so `-c <src> -o <out>` ordering still works.
            continue;
        };
        let _ = out;
    }
    let Some(out_path) = out_path else {
        return false;
    };
    let mut skip_next = false;
    for &arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if flag_takes_value(arg) {
            skip_next = true;
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        if !is_flag_check_companion(out_path, arg) {
            return false;
        }
        saw_flag_src = true;
    }
    saw_flag_src
}

/// Matches the exact `cc-rs` compiler-family detection probe shape: `-E`, no `-o` or extra
/// output-producing flags, no header-search flags, and ONLY ONE positional source input whose
/// basename is `<0+ ascii digits>detect_compiler_family.c`.
fn is_family_probe(args: &[&str]) -> bool {
    if !args.contains(&"-E") {
        return false;
    }
    let mut family_inputs = 0usize;
    let mut skip_next = false;
    for &arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "-o"
            || (arg.starts_with("-o") && arg.len() > 2)
            || is_header_search_flag(arg)
            || is_extra_output_flag(arg)
        {
            return false;
        }
        if flag_takes_value(arg) {
            skip_next = true;
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        let base = name_of(arg);
        let Some(prefix) = base.strip_suffix("detect_compiler_family.c") else {
            return false;
        };
        if !prefix.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        family_inputs += 1;
    }
    family_inputs == 1
}

/// Whether the spawn of `program` with `args` is a compiler probe, from its text alone: a C
/// compiler asked `-?`, running `cc-rs`'s `is_flag_supported` (`-c -o <dir>/flag_check
/// <dir>/flag_check.<c-ext>`), or running `cc-rs`'s zero-header compiler-family detection
/// (`cc -E <dir>/<digits>detect_compiler_family.c` with no `-o` or header-search flags). Real
/// preprocessing or compile jobs stay on the deferred/build C route.
pub fn is_probe(program: &str, args: &[&str]) -> bool {
    // Path-qualified compiler paths (e.g. `/opt/gcc/bin/gcc`, `/missing/cc`) may refer to a
    // distinct or absent binary and must stay on the normal compiler route instead of being
    // redirected by basename to the pinned clang-23 probe answerer.
    if program.contains('/') || !COMPILERS.contains(&program) {
        return false;
    }
    args == ["-?"] || is_flag_check_probe(args) || is_family_probe(args)
}

/// The table's identity for the base key: its bytes, under this form.
pub fn table_id(bytes: &[u8]) -> ContentId {
    let mut hasher = Hasher::new(ID_DOMAIN);
    hasher.update(&FORM.to_le_bytes()).part(bytes);
    hasher.finish()
}

/// What the table says a probe printed.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer<'a> {
    /// Not yet run by clang_answers.rs.
    Pending,
    Known { code: i32, stdout: AString<'a>, stderr: AString<'a> },
}

/// One row: a probe's whole input and its answer.
#[derive(Debug)]
pub struct Row<'a> {
    pub clang: AString<'a>,
    pub program: AString<'a>,
    pub args: AVec<'a, AString<'a>>,
    /// `(name, value)`, the names from [`CLANG_ENV`].
    pub env: AVec<'a, (AString<'a>, AString<'a>)>,
    /// `($IN<n><ext>, sha256 hex)` of each file the probe reads under `OUT_DIR`.
    pub files: AVec<'a, (AString<'a>, AString<'a>)>,
    pub answer: Answer<'a>,
}

/// The table, parsed.
#[derive(Debug)]
pub struct Table<'a> {
    id: ContentId,
    rows: AVec<'a, Row<'a>>,
}

/// Why the table was refused: the byte offset in it and what was wrong there.
#[derive(Debug)]
pub struct TableError {
    pub at: usize,
    pub why: TableWhy,
}

#[derive(Debug)]
pub enum TableWhy {
    Json(blaze_json::What),
    /// `form` is not [`FORM`].
    Form,
    /// The text is not the shape a table has: what was wanted.
    Shape(&'static str),
    /// A key a map already had.
    TwoKeys,
    /// A row whose key an earlier row has (rule 21): its index.
    TwoRows { row: usize },
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{PATH} at byte {}: ", self.at)?;
        match &self.why {
            TableWhy::Json(what) => write!(f, "{what}"),
            TableWhy::Form => write!(f, "`form` is not {FORM}"),
            TableWhy::Shape(wanted) => write!(f, "{wanted} was wanted"),
            TableWhy::TwoKeys => f.write_str("a key the object already has"),
            TableWhy::TwoRows { row } => write!(f, "rows[{row}] has the key of an earlier row: two answers to one probe (rule 21)"),
        }
    }
}

/// A probe the table does not answer.
#[derive(Debug, PartialEq, Eq)]
pub struct ProbeRefused<'a> {
    pub why: ProbeWhy,
    /// The row to add to the table, as one line of JSON with its answer PENDING.
    pub row: AString<'a>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeWhy {
    /// No row has this probe's key.
    NoRow,
    /// Its row has not been answered by clang_answers.rs.
    Pending,
    /// The probe was given standard input, which no row holds.
    Stdin,
    /// The probe references headers, response files, or package paths outside `OUT_DIR`.
    PackageInput,
    /// The probe uses host-CPU or timestamp-observing options/macros not determined at `Level::Inputs`.
    HostDependent,
}

impl fmt::Display for ProbeRefused<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.why {
            ProbeWhy::NoRow => write!(f, "a compiler probe {PATH} has no row for (rule 3: a probe is answered only from the pinned table)")?,
            ProbeWhy::Pending => write!(f, "a compiler probe whose row in {PATH} is still PENDING")?,
            ProbeWhy::Stdin => {
                return write!(f, "a compiler probe given standard input: {PATH} keys no input but the arguments, the environment and the files (rule 21): {}", self.row.as_str());
            }
            ProbeWhy::PackageInput => {
                return write!(f, "a compiler probe references headers or package paths outside OUT_DIR, which a zero-header probe does not key or mount (rule 21): {}", self.row.as_str());
            }
            ProbeWhy::HostDependent => {
                return write!(f, "a compiler probe uses host-CPU or host-timestamp observations not determined at Level::Inputs (rule 21): {}", self.row.as_str());
            }
        }
        write!(f, "; add or keep the row and run scripts/src/bin/clang_answers.rs: {}", self.row.as_str())
    }
}

/// A spawn, as the script made it.
#[derive(Clone, Copy, Debug)]
pub struct Spawn<'s> {
    pub program: &'s str,
    pub args: &'s [&'s str],
    /// The child's environment (any order; only [`CLANG_ENV`] is read).
    pub env: &'s [(&'s str, &'s str)],
    /// The guest `OUT_DIR`, without a trailing `/`.
    pub out_dir: &'s str,
    /// The guest SDK folder; empty where the target has none.
    pub sdk: &'s str,
    /// The script's root mount / package directory (e.g. `/package` or `/workspace/vendored/foo`);
    /// normalized to [`MOUNT`] so workspace-path-dependent flags like `-ffile-prefix-map=...`
    /// share one canonical key. Empty disables mount normalization.
    pub mount: &'s str,
    /// Identity of the SDK (e.g. `"macos-sdk 15.2"`) or pinned Linux sysroot (`""` when none),
    /// hashed into [`ProbeKey`].
    pub sdk_id: &'s str,
    /// What the spawn was given on standard input: a probe answers only when it is empty.
    pub stdin: &'s [u8],
}

/// Content-addressed key of a canonicalized compiler probe.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ProbeKey(pub ContentId);

/// Canonical placeholder-spelled output of a compiler probe, BEFORE [`CanonicalProbe::unspell`].
#[derive(Debug, PartialEq, Eq)]
pub struct ProbeAnswer<'a> {
    pub code: i32,
    pub stdout: AString<'a>,
    pub stderr: AString<'a>,
}

/// Why an encoded probe cache record could not be decoded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProbeDecodeError {
    Truncated,
    BadMagic,
    BadVersion(u32),
    BadUtf8,
    Trailing,
}

impl fmt::Display for ProbeDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProbeDecodeError::Truncated => f.write_str("truncated c-probe record"),
            ProbeDecodeError::BadMagic => f.write_str("c-probe record magic mismatch"),
            ProbeDecodeError::BadVersion(v) => write!(f, "unsupported c-probe record version {v}"),
            ProbeDecodeError::BadUtf8 => f.write_str("c-probe record stream is not UTF-8"),
            ProbeDecodeError::Trailing => f.write_str("trailing bytes in c-probe record"),
        }
    }
}

/// Encodes a canonical [`ProbeAnswer`] into binary `bzcp` v1 format for CAS storage.
pub fn encode_answer(ans: &ProbeAnswer<'_>, out: &mut AVec<'_, u8>) {
    out.extend_from_slice(&ANSWER_MAGIC);
    out.extend_from_slice(&ANSWER_VERSION.to_le_bytes());
    out.extend_from_slice(&ans.code.to_le_bytes());
    let stdout = ans.stdout.as_bytes();
    let stderr = ans.stderr.as_bytes();
    out.extend_from_slice(&u32::try_from(stdout.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(stdout);
    out.extend_from_slice(&u32::try_from(stderr.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(stderr);
}

/// Decodes a binary `bzcp` v1 probe record into a canonical [`ProbeAnswer`].
pub fn decode_answer<'a>(bytes: &[u8], alloc: Alloc<'a>) -> Result<ProbeAnswer<'a>, ProbeDecodeError> {
    let take = |at: &mut usize, n: usize| -> Result<&[u8], ProbeDecodeError> {
        let end = at.checked_add(n).ok_or(ProbeDecodeError::Truncated)?;
        let slice = bytes.get(*at..end).ok_or(ProbeDecodeError::Truncated)?;
        *at = end;
        Ok(slice)
    };
    let take_u32 = |at: &mut usize| -> Result<u32, ProbeDecodeError> {
        let b = take(at, 4)?;
        let arr: [u8; 4] = b.try_into().map_err(|_| ProbeDecodeError::Truncated)?;
        Ok(u32::from_le_bytes(arr))
    };
    let mut at = 0usize;
    let magic = take(&mut at, 4)?;
    if magic != ANSWER_MAGIC {
        return Err(ProbeDecodeError::BadMagic);
    }
    let version = take_u32(&mut at)?;
    if version != ANSWER_VERSION {
        return Err(ProbeDecodeError::BadVersion(version));
    }
    let code_bytes: [u8; 4] = take(&mut at, 4)?.try_into().map_err(|_| ProbeDecodeError::Truncated)?;
    let code = i32::from_le_bytes(code_bytes);
    let out_len = usize::try_from(take_u32(&mut at)?).map_err(|_| ProbeDecodeError::Truncated)?;
    let out_raw = take(&mut at, out_len)?;
    let out_str = core::str::from_utf8(out_raw).map_err(|_| ProbeDecodeError::BadUtf8)?;
    let err_len = usize::try_from(take_u32(&mut at)?).map_err(|_| ProbeDecodeError::Truncated)?;
    let err_raw = take(&mut at, err_len)?;
    let err_str = core::str::from_utf8(err_raw).map_err(|_| ProbeDecodeError::BadUtf8)?;
    if at != bytes.len() {
        return Err(ProbeDecodeError::Trailing);
    }
    Ok(ProbeAnswer {
        code,
        stdout: String::from_str_in(out_str, alloc),
        stderr: String::from_str_in(err_str, alloc),
    })
}

/// Digest of a canonical [`ProbeAnswer`] (computed BEFORE [`CanonicalProbe::unspell`]), recorded
/// in the script's read list under `cc-probe\n<key hex>` so `fact_now` can re-verify without `OUT_DIR`.
pub fn canonical_reply_digest(ans: &ProbeAnswer<'_>) -> ContentId {
    let mut hash = Hasher::new(REPLY_DOMAIN);
    hash.update(&ans.code.to_le_bytes());
    for part in [ans.stdout.as_bytes(), ans.stderr.as_bytes()] {
        hash.update(&u32::try_from(part.len()).unwrap_or(u32::MAX).to_le_bytes()).update(part);
    }
    hash.finish()
}

/// A structural piece of a canonicalized probe argument or environment value: separates real
/// guest-path substitutions from literal text (e.g. `-DROOT="$OUT_DIR"`) so they never collide in
/// [`ProbeKey`] or get mistakenly expanded at execution time.
#[derive(Debug, PartialEq, Eq)]
pub enum Segment<'b> {
    Literal(AString<'b>),
    OutDir,
    SdkRoot,
    Mount,
    Placeholder(AString<'b>),
}

/// A canonicalized compiler probe ready for cache lookup or execution by a [`ProbeAnswerer`].
pub struct CanonicalProbe<'b, 's> {
    pub key: ProbeKey,
    pub answerer_id: ContentId,
    pub clang: [u8; 32],
    pub sdk_id: AString<'b>,
    pub row: Row<'b>,
    /// Structural argument segments (`Segment::Literal` vs path placeholders).
    pub exec_args: AVec<'b, AVec<'b, Segment<'b>>>,
    /// Structural environment `(name, segments)`.
    pub exec_env: AVec<'b, (AString<'b>, AVec<'b, Segment<'b>>)>,
    /// `($IN<n><ext> or $OUT<n><ext>, original spawned arg)`.
    pub paths: AVec<'b, (AString<'b>, &'s str)>,
    /// `($IN<n><ext>, OUT_DIR-relative path)` for every input file present in `OUT_DIR`.
    pub input_rels: AVec<'b, (AString<'b>, AString<'b>)>,
}

impl<'b> CanonicalProbe<'b, '_> {
    /// Restores placeholder paths (`$IN<n>`, `$OUT<n>`, `$OUT_DIR`, `$SDKROOT`, `$MOUNT`) back
    /// into the script's guest paths in a single left-to-right pass, unescaping `$$` -> literal
    /// `$` so literal text like `$OUT_DIR` or `$IN0.c` emitted by Clang is preserved verbatim.
    pub fn unspell(&self, text: &str, spawn: &Spawn<'_>, alloc: Alloc<'b>) -> AString<'b> {
        let mut pairs: AVec<'b, (&str, &str)> = Vec::new_in(alloc);
        for (spelled, actual) in self.paths.iter() {
            pairs.push((spelled.as_str(), *actual));
        }
        pairs.sort_unstable_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(b.0)));
        pairs.push((OUT_DIR, spawn.out_dir));
        if !spawn.sdk.is_empty() {
            pairs.push((SDKROOT, spawn.sdk));
        }
        if !spawn.mount.is_empty() {
            pairs.push((MOUNT, spawn.mount));
        }
        let mut out = String::new_in(alloc);
        let mut at = 0usize;
        'scan: while let Some(rest) = text.get(at..) {
            if rest.is_empty() {
                break;
            }
            if rest.starts_with("$$") {
                out.push('$');
                at += 2;
                continue;
            }
            for (from, to) in pairs.iter().copied() {
                if !from.is_empty() && rest.starts_with(from) {
                    out.push_str(to);
                    at += from.len();
                    continue 'scan;
                }
            }
            if let Some(ch) = rest.chars().next() {
                out.push(ch);
                at += ch.len_utf8();
            } else {
                break;
            }
        }
        out
    }
}

/// Whether `arg` is a safe string-rewriting prefix-map flag that never reads package files.
fn is_prefix_map_flag(arg: &str) -> bool {
    arg.starts_with("-ffile-prefix-map=")
        || arg.starts_with("-fdebug-prefix-map=")
        || arg.starts_with("-fmacro-prefix-map=")
        || arg.starts_with("-fcoverage-prefix-map=")
}

/// Whether `arg` sets only output-object compilation-directory metadata (never reads that directory).
fn is_metadata_dir_flag(arg: &str) -> bool {
    arg.starts_with("-fdebug-compilation-dir=")
        || arg.starts_with("-fcoverage-compilation-dir=")
        || arg.starts_with("-ffile-compilation-dir=")
}

/// Environment variables in [`CLANG_ENV`] that steer header/file discovery outside `OUT_DIR`.
fn is_path_steering_env(name: &str) -> bool {
    matches!(
        name,
        "CPATH"
            | "C_INCLUDE_PATH"
            | "CPLUS_INCLUDE_PATH"
            | "OBJC_INCLUDE_PATH"
            | "OBJCPLUS_INCLUDE_PATH"
            | "COMPILER_PATH"
            | "CCC_OVERRIDE_OPTIONS"
    )
}

/// Compiler flags that take the next argv element as a non-source value.
fn flag_takes_value(arg: &str) -> bool {
    matches!(
        arg,
        "-o" | "-x"
            | "-target"
            | "--target"
            | "-arch"
            | "-mcpu"
            | "-march"
            | "-mtune"
            | "-isysroot"
            | "--sysroot"
            | "-mllvm"
            | "-Xclang"
            | "-Xassembler"
            | "-Xlinker"
            | "-D"
            | "-U"
            | "-u"
            | "-L"
            | "-l"
            | "-F"
            | "-framework"
            | "-MF"
            | "-MT"
            | "-MQ"
    )
}

/// Environment variables that alter Clang's stderr/stdout/file output or dynamic-linker search
/// outside [`CLANG_ENV`] (e.g. `CC_PRINT_OPTIONS`, `CC_PRINT_OPTIONS_FILE`, `CC_PRINT_HEADERS`,
/// `CC_LOG_DIAGNOSTICS`, `CCC_ADD_ARGS`, `FORCE_CLANG_DIAGNOSTICS_COLOR`, `LLVM_PROFILE_FILE`,
/// `GCOV_PREFIX*`, `LD_*`, `DYLD_*`).
fn is_unmodeled_clang_control_env(name: &str) -> bool {
    matches!(
        name,
        "FORCE_CLANG_DIAGNOSTICS_COLOR"
            | "LLVM_PROFILE_FILE"
            | "GCOV_PREFIX"
            | "GCOV_PREFIX_STRIP"
            | "GLIBC_TUNABLES"
            | "GCONV_PATH"
            | "LOCPATH"
    ) || name.starts_with("CC_PRINT_")
        || name.starts_with("CC_LOG_")
        || (name.starts_with("CCC_") && name != "CCC_OVERRIDE_OPTIONS")
        || name.starts_with("CLANG_")
        || name.starts_with("LD_")
        || name.starts_with("DYLD_")
}

/// Whether `arg` (and optional `next`) requests host CPU auto-detection, ambient toolchain
/// discovery (`-v`, `-###`, `--verbose`, `-print-*`, `--print-*`, `-dump*`), profile feedback
/// input, or nondeterministic compiler timing/stat reports not determined at `Level::Inputs`.
/// Deterministic instrumentation-generation flags such as `-fprofile-instr-generate` and
/// `-fprofile-arcs` are allowed in flag-support probes.
fn is_host_dependent_flag(arg: &str, next: Option<&str>) -> bool {
    if matches!(
        arg,
        "-march=native"
            | "-mcpu=native"
            | "-mtune=native"
            | "--target=native"
            | "-v"
            | "-###"
            | "--verbose"
            | "-dumpmachine"
            | "-dumpversion"
            | "-dumpfullversion"
            | "-print-stats"
            | "--print-stats"
            | "-time-passes"
            | "-stats"
    ) || arg.starts_with("-print-")
        || arg.starts_with("--print-")
        || arg.starts_with("-Xarch_host")
        || arg.starts_with("-fprofile-use")
        || arg.starts_with("-fprofile-instr-use")
        || arg.starts_with("-fprofile-sample-use")
        || arg.starts_with("-fprofile-remapping-file")
        || arg.starts_with("-fauto-profile")
        || arg.starts_with("-ftime-report")
        || arg.starts_with("-ftime-trace")
        || arg.starts_with("-fproc-stat-report")
    {
        return true;
    }
    if matches!(arg, "-march" | "-mcpu" | "-mtune" | "-target" | "--target") && next == Some("native") {
        return true;
    }
    false
}

/// Whether `text` contains a disallowed preprocessor/macro/filename/assembly token in either
/// probe source text or command-line `-D` macro arguments.
fn contains_disallowed_probe_token(text: &str) -> bool {
    text.contains("##")
        || text.contains("%:%:")
        || contains_ident(text, "__has_include")
        || contains_ident(text, "__has_include_next")
        || contains_ident(text, "__has_embed")
        || contains_ident(text, "_Pragma")
        || contains_ident(text, "__FILE__")
        || contains_ident(text, "__BASE_FILE__")
        || contains_ident(text, "__FILE_NAME__")
        || contains_ident(text, "__builtin_FILE")
        || contains_ident(text, "__builtin_FILE_NAME")
        || contains_ident(text, "__builtin_source_location")
        || contains_ident(text, "asm")
        || contains_ident(text, "__asm")
        || contains_ident(text, "__asm__")
}

/// Whether `arg` names an assembler source file (`.s`, `.S`, `.asm`) or explicit assembler
/// language selection (`-x assembler*` / `-xassembler*`).
fn is_assembler_input_or_lang(arg: &str, prev: Option<&str>) -> bool {
    if prev == Some("-x") && arg.starts_with("assembler") {
        return true;
    }
    if arg.starts_with("-xassembler") {
        return true;
    }
    if !arg.starts_with('-')
        && (arg.ends_with(".s") || arg.ends_with(".S") || arg.ends_with(".asm") || arg.ends_with(".ASM"))
    {
        return true;
    }
    false
}

/// Whether `text` contains identifier token `ident` delimited by non-identifier characters.
fn contains_ident(text: &str, ident: &str) -> bool {
    let bytes = text.as_bytes();
    let is_id = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut start = 0usize;
    while let Some(rest) = text.get(start..) {
        let Some(pos) = rest.find(ident) else {
            return false;
        };
        let abs = start + pos;
        let left_ok = abs == 0 || !bytes.get(abs - 1).is_some_and(|&b| is_id(b));
        let right_ok = !bytes.get(abs + ident.len()).is_some_and(|&b| is_id(b));
        if left_ok && right_ok {
            return true;
        }
        start = abs + ident.len();
    }
    false
}

/// Masks characters inside `"..."` and `'...'` literals with spaces while preserving quote
/// delimiters so inert string tokens (e.g. `-DVERSION="__TIMESTAMP__"`) are not misclassified as
/// active preprocessor operators.
fn mask_quoted_literals<'a>(text: &str, alloc: Alloc<'a>) -> AString<'a> {
    let mut out: AVec<'a, u8> = Vec::with_capacity_in(text.len(), alloc);
    let src = text.as_bytes();
    let mut j = 0usize;
    let mut quote: Option<u8> = None;
    while let Some(&b) = src.get(j) {
        if let Some(q) = quote {
            j += 1;
            if b == b'\\' {
                out.push(b' ');
                if src.get(j).is_some() {
                    out.push(b' ');
                    j += 1;
                }
            } else if b == q {
                out.push(b);
                quote = None;
            } else if b == b'\n' {
                out.push(b'\n');
                quote = None;
            } else {
                out.push(b' ');
            }
            continue;
        }
        if b == b'"' || b == b'\'' {
            quote = Some(b);
            out.push(b);
            j += 1;
            continue;
        }
        out.push(b);
        j += 1;
    }
    match core::str::from_utf8(out.as_slice()) {
        Ok(s) => String::from_str_in(s, alloc),
        Err(_) => String::from_str_in(text, alloc),
    }
}

/// Checks materialized `OUT_DIR` probe source bytes (after C translation phase 2 line-splicing
/// and phase 3 comment stripping) for unkeyed file/header inclusions (`#include`, `#include_next`,
/// `#import`, `__has_include`, `__has_include_next`, `__has_embed`) or host-timestamp macros
/// (`__TIMESTAMP__`), returning the refusal reason if disallowed.
fn disallowed_probe_source(bytes: &[u8], alloc: Alloc<'_>) -> Option<ProbeWhy> {
    let Ok(raw_text) = core::str::from_utf8(bytes) else {
        return Some(ProbeWhy::PackageInput);
    };
    // Conservatively reject trigraphs that introduce `#` or `\` (`??=`, `??/`).
    if raw_text.contains("??=") || raw_text.contains("??/") {
        return Some(ProbeWhy::PackageInput);
    }
    // Phase 2: remove `\\\n` and `\\\r\n` line splices.
    let mut spliced: AVec<'_, u8> = Vec::with_capacity_in(bytes.len(), alloc);
    let mut i = 0usize;
    while let Some(&b) = bytes.get(i) {
        if b == b'\\' {
            if bytes.get(i + 1) == Some(&b'\n') {
                i += 2;
                continue;
            }
            if bytes.get(i + 1) == Some(&b'\r') && bytes.get(i + 2) == Some(&b'\n') {
                i += 3;
                continue;
            }
        }
        spliced.push(b);
        i += 1;
    }
    // Phase 3: strip `/* ... */` (replaced by a single space) and `// ...` comments, and mask
    // characters inside string/char literals with spaces (preserving quotes so `#include "..."`
    // still parses while inert literal text like `"__has_include"` or `"__TIMESTAMP__"` is not
    // misclassified as an active preprocessor operator). An unclosed block comment is rejected
    // conservatively.
    let mut norm: AVec<'_, u8> = Vec::with_capacity_in(spliced.len(), alloc);
    let mut j = 0usize;
    let mut quote: Option<u8> = None;
    let src = spliced.as_slice();
    while let Some(&b) = src.get(j) {
        if let Some(q) = quote {
            j += 1;
            if b == b'\\' {
                norm.push(b' ');
                if src.get(j).is_some() {
                    norm.push(b' ');
                    j += 1;
                }
            } else if b == q {
                norm.push(b);
                quote = None;
            } else if b == b'\n' {
                norm.push(b'\n');
                quote = None;
            } else {
                norm.push(b' ');
            }
            continue;
        }
        if b == b'"' || b == b'\'' {
            quote = Some(b);
            norm.push(b);
            j += 1;
            continue;
        }
        if b == b'/' && src.get(j + 1) == Some(&b'*') {
            j += 2;
            let mut closed = false;
            while let Some(&cb) = src.get(j) {
                if cb == b'*' && src.get(j + 1) == Some(&b'/') {
                    j += 2;
                    closed = true;
                    break;
                }
                j += 1;
            }
            if !closed {
                return Some(ProbeWhy::PackageInput);
            }
            norm.push(b' ');
            continue;
        }
        if b == b'/' && src.get(j + 1) == Some(&b'/') {
            j += 2;
            while let Some(&cb) = src.get(j) {
                if cb == b'\n' {
                    break;
                }
                j += 1;
            }
            continue;
        }
        norm.push(b);
        j += 1;
    }
    let Ok(text) = core::str::from_utf8(norm.as_slice()) else {
        return Some(ProbeWhy::PackageInput);
    };
    for line in text.lines() {
        let trimmed = line.trim_start_matches([' ', '\t', '\r', '\x0b', '\x0c']);
        // Reject assembler file-inclusion directives (`.include` / `.incbin`) anywhere in a
        // probe input (including after `;` statement separators).
        for stmt in trimmed.split(';') {
            let s = stmt.trim_start_matches([' ', '\t', '\r', '\x0b', '\x0c']);
            if let Some(after_dot) = s.strip_prefix('.') {
                for kw in ["include", "incbin"] {
                    if let Some(rest) = after_dot.strip_prefix(kw) {
                        let boundary = rest.bytes().next().is_none_or(|b| !b.is_ascii_alphanumeric() && b != b'_');
                        if boundary {
                            return Some(ProbeWhy::PackageInput);
                        }
                    }
                }
            }
        }
        let after_hash = trimmed.strip_prefix('#').or_else(|| trimmed.strip_prefix("%:"));
        if let Some(after_hash) = after_hash {
            let dir = after_hash.trim_start_matches([' ', '\t', '\r', '\x0b', '\x0c']);
            for kw in ["include_next", "include", "import", "embed"] {
                if let Some(rest) = dir.strip_prefix(kw) {
                    let boundary = rest.bytes().next().is_none_or(|b| !b.is_ascii_alphanumeric() && b != b'_');
                    if boundary {
                        return Some(ProbeWhy::PackageInput);
                    }
                }
            }
            // Allow only `#pragma message` (used by `cc`'s `detect_compiler_family.c`); refuse
            // every other `#pragma` (such as `#pragma GCC dependency` or `#pragma include_alias`).
            if let Some(rest) = dir.strip_prefix("pragma")
                && rest.bytes().next().is_none_or(|b| !b.is_ascii_alphanumeric() && b != b'_')
            {
                let after = rest.trim_start_matches([' ', '\t', '\r', '\x0b', '\x0c']);
                let is_message = after
                    .strip_prefix("message")
                    .is_some_and(|tail| tail.bytes().next().is_none_or(|b| !b.is_ascii_alphanumeric() && b != b'_'));
                if !is_message {
                    return Some(ProbeWhy::PackageInput);
                }
            }
        }
    }
    if contains_disallowed_probe_token(text) {
        return Some(ProbeWhy::PackageInput);
    }
    if contains_clock_macro(text) {
        return Some(ProbeWhy::HostDependent);
    }
    None
}

/// Builds a [`CanonicalProbe`] and its [`ProbeKey`] from a script spawn.

/// Whether `text` references a wall-clock macro whose expansion depends on the host clock (or on
/// Blaze injecting `SOURCE_DATE_EPOCH=0` when the caller did not set one).
fn contains_clock_macro(text: &str) -> bool {
    contains_ident(text, "__TIMESTAMP__")
        || contains_ident(text, "__DATE__")
        || contains_ident(text, "__TIME__")
}
pub fn canonicalize<'b, 's>(
    answerer_id: ContentId,
    clang: &[u8; 32],
    spawn: &Spawn<'s>,
    mut file: impl FnMut(&str) -> Option<[u8; 32]>,
    alloc: Alloc<'b>,
) -> Result<CanonicalProbe<'b, 's>, ProbeRefused<'b>> {
    let clang_hex = hex(clang, alloc);
    let asked = asked(clang_hex.as_str(), spawn, &mut file, alloc);
    if !spawn.stdin.is_empty() {
        return Err(ProbeRefused { why: ProbeWhy::Stdin, row: row_json(&asked.row, alloc) });
    }
    let has_env_package_input = asked
        .row
        .env
        .iter()
        .any(|(k, v)| is_path_steering_env(k.as_str()) && !v.as_str().is_empty());
    let mut has_arg_package_input = false;
    let mut has_host_dep_arg = spawn
        .env
        .iter()
        .any(|(k, v)| is_unmodeled_clang_control_env(k) && !v.is_empty());
    let mut prev_flag: Option<&str> = None;
    for (idx, &arg) in spawn.args.iter().enumerate() {
        let next = spawn.args.get(idx + 1).copied();
        if is_host_dependent_flag(arg, next) {
            has_host_dep_arg = true;
            break;
        }
        if let Some(prev) = prev_flag.take() {
            if prev == "-D" {
                let masked_arg = mask_quoted_literals(arg, alloc);
                let masked = masked_arg.as_str();
                if contains_clock_macro(masked) {
                    has_host_dep_arg = true;
                    break;
                }
                if contains_disallowed_probe_token(masked) {
                    has_arg_package_input = true;
                    break;
                }
                // Separate `-D 'ROOT="/literal"'` has passed macro-token checks; preserve path
                // literals just like attached `-DROOT="/literal"` below.
                continue;
            }
            // Reject nested response files (e.g. `-Xassembler @args.rsp`) and assembler language
            // selection (`-x assembler`). Only `-o` may carry an `OUT_DIR` path; any other flag
            // taking a separate value with a path separator (e.g. `-L /host/lib`) is refused as
            // an unkeyed external input.
            if arg.starts_with('@') || is_assembler_input_or_lang(arg, Some(prev)) {
                has_arg_package_input = true;
                break;
            }
            if prev == "-o" {
                if arg != "/dev/null" && !is_under_out_dir(arg, spawn.out_dir) {
                    has_arg_package_input = true;
                    break;
                }
            } else if arg.contains('/') || arg.contains('\\') {
                has_arg_package_input = true;
                break;
            }
            continue;
        }
        if let Some(macro_def) = arg.strip_prefix("-D")
            && !macro_def.is_empty()
        {
            let masked_arg = mask_quoted_literals(macro_def, alloc);
            let masked = masked_arg.as_str();
            if contains_clock_macro(masked) {
                has_host_dep_arg = true;
                break;
            }
            if contains_disallowed_probe_token(masked) {
                has_arg_package_input = true;
                break;
            }
        }
        if is_header_search_flag(arg)
            || arg.starts_with('@')
            || is_assembler_input_or_lang(arg, None)
            || (!spawn.mount.is_empty() && arg.contains(spawn.mount) && !is_prefix_map_flag(arg))
        {
            has_arg_package_input = true;
            break;
        }
        if flag_takes_value(arg) {
            prev_flag = Some(arg);
            continue;
        }
        if !arg.starts_with('-') && arg != "/dev/null" && !is_under_out_dir(arg, spawn.out_dir) {
            // Relative source path (`probe.c`) or absolute path outside `OUT_DIR`.
            has_arg_package_input = true;
            break;
        }
    }
    let uses_unbound_sdk = spawn.sdk_id.is_empty()
        && (asked.exec_args.iter().flatten().any(|s| matches!(s, Segment::SdkRoot))
            || asked.exec_env.iter().flat_map(|(_, segs)| segs.iter()).any(|s| matches!(s, Segment::SdkRoot)));
    if has_host_dep_arg {
        return Err(ProbeRefused { why: ProbeWhy::HostDependent, row: row_json(&asked.row, alloc) });
    }
    if has_env_package_input || has_arg_package_input || uses_unbound_sdk {
        return Err(ProbeRefused { why: ProbeWhy::PackageInput, row: row_json(&asked.row, alloc) });
    }
    let key = probe_key(answerer_id, clang, spawn.sdk_id, &asked.row, &asked.exec_args, &asked.exec_env);
    Ok(CanonicalProbe {
        key,
        answerer_id,
        clang: *clang,
        sdk_id: String::from_str_in(spawn.sdk_id, alloc),
        row: asked.row,
        exec_args: asked.exec_args,
        exec_env: asked.exec_env,
        paths: asked.paths,
        input_rels: asked.input_rels,
    })
}

fn hash_segments(h: &mut Hasher, segs: &[Segment<'_>]) {
    h.update(&u32::try_from(segs.len()).unwrap_or(u32::MAX).to_le_bytes());
    for seg in segs {
        match seg {
            Segment::Literal(s) => {
                h.update(&[0u8]).part(s.as_bytes());
            }
            Segment::OutDir => {
                h.update(&[1u8]);
            }
            Segment::SdkRoot => {
                h.update(&[2u8]);
            }
            Segment::Mount => {
                h.update(&[3u8]);
            }
            Segment::Placeholder(p) => {
                h.update(&[4u8]).part(p.as_bytes());
            }
        }
    }
}

fn probe_key(
    answerer_id: ContentId,
    clang: &[u8; 32],
    sdk_id: &str,
    row: &Row<'_>,
    exec_args: &[AVec<'_, Segment<'_>>],
    exec_env: &[(AString<'_>, AVec<'_, Segment<'_>>)],
) -> ProbeKey {
    let mut h = Hasher::new(PROBE_KEY_DOMAIN);
    h.id(&answerer_id).part(clang).part(sdk_id.as_bytes()).part(row.program.as_bytes());
    h.update(&u32::try_from(exec_args.len()).unwrap_or(u32::MAX).to_le_bytes());
    for segs in exec_args {
        hash_segments(&mut h, segs.as_slice());
    }
    h.update(&u32::try_from(exec_env.len()).unwrap_or(u32::MAX).to_le_bytes());
    for (k, segs) in exec_env {
        h.part(k.as_bytes());
        hash_segments(&mut h, segs.as_slice());
    }
    h.update(&u32::try_from(row.files.len()).unwrap_or(u32::MAX).to_le_bytes());
    for (k, v) in row.files.iter() {
        h.part(k.as_bytes()).part(v.as_bytes());
    }
    ProbeKey(h.finish())
}

/// Strategy seam (rule 78) for answering a canonical compiler probe: first instantiation is
/// hermetic pinned `clang-23` (`HermeticClangProbe`), future instantiation is `blaze-cfront`.
pub trait ProbeAnswerer {
    /// Stable identity of this answerer strategy and its confinement/format version (hashed into
    /// [`ProbeKey`]). Fallible because deriving a host runtime-closure digest can fail if a
    /// required library or link cannot be read within the probe deadline.
    fn id<'a>(&self, alloc: Alloc<'a>) -> Result<ContentId, AString<'a>>;
    /// Produces the canonical placeholder-spelled [`ProbeAnswer`] for `probe`.
    fn answer<'a, 'b>(
        &self,
        probe: &CanonicalProbe<'_, '_>,
        input_bytes: &mut dyn FnMut(&str) -> Option<&'b [u8]>,
        alloc: Alloc<'a>,
    ) -> Result<ProbeAnswer<'a>, AString<'a>>;
}

/// Content-addressed cache of encoded [`ProbeAnswer`] records keyed by [`ProbeKey`].
pub trait ProbeCache {
    fn get<'a>(&self, key: &ProbeKey, alloc: Alloc<'a>) -> Result<Option<AVec<'a, u8>>, AString<'a>>;
    fn put<'a>(&self, key: &ProbeKey, bytes: &[u8], alloc: Alloc<'a>) -> Result<(), AString<'a>>;
}

/// A resolved probe: its canonical key, answerer strategy identity, and reply digest (for the
/// script's per-probe read list) plus the guest-spelled [`Answered`] output delivered to the
/// script.
#[derive(Debug, PartialEq, Eq)]
pub struct ResolvedProbe<'a> {
    pub key: ProbeKey,
    pub answerer_id: ContentId,
    pub canonical_digest: ContentId,
    pub answered: Answered<'a>,
}

/// Resolves a compiler probe through one [`ProbeAnswerer`] and one [`ProbeCache`]: checks the
/// cache first, invokes `answerer` on miss and persists the canonical answer, then computes
/// `canonical_digest` before `unspell`.
pub fn resolve<'a, 'b>(
    answerer: &dyn ProbeAnswerer,
    cache: &dyn ProbeCache,
    clang: &[u8; 32],
    spawn: &Spawn<'_>,
    file_digest: impl FnMut(&str) -> Option<[u8; 32]>,
    file_bytes: &mut dyn FnMut(&str) -> Option<&'b [u8]>,
    alloc: Alloc<'a>,
) -> Result<ResolvedProbe<'a>, AString<'a>> {
    let answerer_id = answerer.id(alloc)?;
    let probe = canonicalize(answerer_id, clang, spawn, file_digest, alloc).map_err(|r| {
        let mut msg = String::new_in(alloc);
        let _ = core::fmt::Write::write_fmt(&mut msg, format_args!("{r}"));
        msg
    })?;
    for (_, rel) in probe.input_rels.iter() {
        if let Some(bytes) = file_bytes(rel.as_str())
            && let Some(why) = disallowed_probe_source(bytes, alloc)
        {
            let refused = ProbeRefused { why, row: row_json(&probe.row, alloc) };
            let mut msg = String::new_in(alloc);
            let _ = core::fmt::Write::write_fmt(&mut msg, format_args!("{refused}"));
            return Err(msg);
        }
    }
    let canonical = match cache.get(&probe.key, alloc)? {
        Some(bytes) => {
            // Revalidate the live answerer identity after the cache read so a runtime-closure
            // switch during cache lookup never serves a stale record under an obsolete identity.
            let post_id = answerer.id(alloc)?;
            if post_id != answerer_id {
                let mut msg = String::new_in(alloc);
                let _ = core::fmt::Write::write_fmt(
                    &mut msg,
                    format_args!("c-probe answerer runtime closure changed during cache lookup (rule 21)"),
                );
                return Err(msg);
            }
            decode_answer(bytes.as_slice(), alloc).map_err(|e| {
                let mut msg = String::new_in(alloc);
                let _ = core::fmt::Write::write_fmt(&mut msg, format_args!("cached c-probe record: {e}"));
                msg
            })?
        }
        None => {
            let ans = answerer.answer(&probe, file_bytes, alloc)?;
            let mut encoded: AVec<'a, u8> = Vec::new_in(alloc);
            encode_answer(&ans, &mut encoded);
            cache.put(&probe.key, encoded.as_slice(), alloc)?;
            ans
        }
    };
    let canonical_digest = canonical_reply_digest(&canonical);
    let answered = Answered {
        code: canonical.code,
        stdout: probe.unspell(canonical.stdout.as_str(), spawn, alloc),
        stderr: probe.unspell(canonical.stderr.as_str(), spawn, alloc),
    };
    Ok(ResolvedProbe { key: probe.key, answerer_id, canonical_digest, answered })
}

/// Like [`same_key`], also matching legacy `/package` rows against `$MOUNT`-normalized args.
fn same_key_mount_compat(table_row: &Row<'_>, asked_row: &Row<'_>) -> bool {
    if same_key(table_row, asked_row) {
        return true;
    }
    let same_set = |x: &[(AString<'_>, AString<'_>)], y: &[(AString<'_>, AString<'_>)]| {
        x.len() == y.len() && x.iter().all(|(k, v)| y.iter().any(|(k2, v2)| k.as_str() == k2.as_str() && v.as_str() == v2.as_str()))
    };
    let arg_eq = |t: &str, a: &str| {
        if t == a {
            return true;
        }
        if let Some((prefix, suffix)) = a.split_once(MOUNT) {
            t.strip_prefix(prefix).and_then(|rest| rest.strip_suffix(suffix)) == Some("/package")
        } else {
            false
        }
    };
    table_row.clang.as_str() == asked_row.clang.as_str()
        && table_row.program.as_str() == asked_row.program.as_str()
        && table_row.args.len() == asked_row.args.len()
        && table_row.args.iter().zip(asked_row.args.iter()).all(|(x, y)| arg_eq(x.as_str(), y.as_str()))
        && same_set(&table_row.env, &asked_row.env)
        && same_set(&table_row.files, &asked_row.files)
}

/// The table's answer to a probe.
#[derive(Debug, PartialEq, Eq)]
pub struct Answered<'a> {
    pub code: i32,
    pub stdout: AString<'a>,
    pub stderr: AString<'a>,
}

impl<'a> Table<'a> {
    /// The table in `bytes`, every row checked: its shape, `form`, and no two rows with one key.
    pub fn parse(bytes: &[u8], alloc: Alloc<'a>) -> Result<Table<'a>, TableError> {
        let reader = Reader::new_in(bytes, Limits::SHIPPED, alloc).map_err(|e| TableError { at: e.at, why: TableWhy::Json(e.what) })?;
        let mut parse = Parse { reader, alloc, at: 0 };
        let mut rows: AVec<'a, Row<'a>> = Vec::new_in(alloc);
        parse.want_map()?;
        let mut seen = [false; 5];
        while let Some(key) = parse.key()? {
            let slot = match key {
                "comment" => 0,
                "form" => 1,
                "made_by" => 2,
                "sources" => 3,
                "rows" => 4,
                _ => return Err(parse.shape("a key of comment, form, made_by, sources, rows")),
            };
            if core::mem::replace(&mut seen[slot], true) {
                return Err(parse.error(TableWhy::TwoKeys));
            }
            match slot {
                1 => match parse.next()? {
                    Event::Int(n) if n.as_u64() == Some(FORM) => {}
                    _ => return Err(parse.error(TableWhy::Form)),
                },
                3 => {
                    parse.want_map()?;
                    while parse.key()?.is_some() {
                        parse.text()?;
                    }
                }
                4 => {
                    parse.want(Event::StartList, "a list of rows")?;
                    while let Some(row) = parse.row()? {
                        if rows.iter().any(|it| same_key(it, &row)) {
                            return Err(parse.error(TableWhy::TwoRows { row: rows.len() }));
                        }
                        rows.push(row);
                    }
                }
                _ => {
                    parse.text()?;
                }
            }
        }
        if !seen[1] || !seen[4] {
            return Err(parse.shape("`form` and `rows`"));
        }
        if parse.reader.next().map_err(|e| TableError { at: e.at, why: TableWhy::Json(e.what) })?.is_some() {
            return Err(parse.shape("the end of the text"));
        }
        Ok(Table { id: table_id(bytes), rows })
    }

    pub fn id(&self) -> ContentId {
        self.id
    }

    pub fn rows(&self) -> &[Row<'a>] {
        &self.rows
    }

    /// The answer to `spawn` (a probe: [`is_probe`]) from the rows of `clang` (this host's clang
    /// archive sha256, `toolchain::clang_pin`'s). `file` gives the sha256 of a file under
    /// `OUT_DIR` by its `OUT_DIR`-relative path (`out-x/1a.c`), or `None` when there is none.
    /// Sans-IO.
    pub fn answer<'b>(
        &self,
        clang: &[u8; 32],
        spawn: &Spawn<'_>,
        mut file: impl FnMut(&str) -> Option<[u8; 32]>,
        alloc: Alloc<'b>,
    ) -> Result<Answered<'b>, ProbeRefused<'b>> {
        let clang = hex(clang, alloc);
        let asked = asked(clang.as_str(), spawn, &mut file, alloc);
        if !spawn.stdin.is_empty() {
            return Err(ProbeRefused { why: ProbeWhy::Stdin, row: row_json(&asked.row, alloc) });
        }
        let Some(row) = self.rows.iter().find(|row| same_key_mount_compat(row, &asked.row)) else {
            return Err(ProbeRefused { why: ProbeWhy::NoRow, row: row_json(&asked.row, alloc) });
        };
        let Answer::Known { code, stdout, stderr } = &row.answer else {
            return Err(ProbeRefused { why: ProbeWhy::Pending, row: row_json(&asked.row, alloc) });
        };
        let back = |text: &str| asked.unspell(text, spawn, alloc);
        Ok(Answered { code: *code, stdout: back(stdout.as_str()), stderr: back(stderr.as_str()) })
    }
}

/// A probe's key as a row, its path placeholders, and its `OUT_DIR`-relative input file paths.
struct Asked<'b, 's> {
    row: Row<'b>,
    exec_args: AVec<'b, AVec<'b, Segment<'b>>>,
    exec_env: AVec<'b, (AString<'b>, AVec<'b, Segment<'b>>)>,
    /// `($IN<n><ext> or $OUT<n><ext>, the argument as spawned)`.
    paths: AVec<'b, (AString<'b>, &'s str)>,
    /// `($IN<n><ext>, OUT_DIR-relative path)` for every input file present in `OUT_DIR`.
    input_rels: AVec<'b, (AString<'b>, AString<'b>)>,
}

impl<'b> Asked<'b, '_> {
    /// `text` (an answer as the table spells it) as the script's guest paths spell it: each
    /// placeholder, longest first (`$IN10` before `$IN1`), then the folders.
    fn unspell(&self, text: &str, spawn: &Spawn<'_>, alloc: Alloc<'b>) -> AString<'b> {
        let mut pairs: AVec<'b, (&str, &str)> = Vec::new_in(alloc);
        for (spelled, actual) in self.paths.iter() {
            pairs.push((spelled.as_str(), *actual));
        }
        pairs.sort_unstable_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(b.0)));
        pairs.push((OUT_DIR, spawn.out_dir));
        if !spawn.sdk.is_empty() {
            pairs.push((SDKROOT, spawn.sdk));
        }
        if !spawn.mount.is_empty() {
            pairs.push((MOUNT, spawn.mount));
        }
        replaced(text, &pairs, alloc)
    }
}

fn asked<'b, 's>(clang: &str, spawn: &Spawn<'s>, file: &mut impl FnMut(&str) -> Option<[u8; 32]>, alloc: Alloc<'b>) -> Asked<'b, 's> {
    let own = |text: &str| String::from_str_in(text, alloc);
    let mut roots: AVec<'b, (&str, &str)> = Vec::new_in(alloc);
    roots.push((spawn.out_dir, OUT_DIR));
    if !spawn.sdk.is_empty() {
        roots.push((spawn.sdk, SDKROOT));
    }
    if !spawn.mount.is_empty() {
        roots.push((spawn.mount, MOUNT));
    }
    roots.sort_unstable_by(|a, b| b.0.len().cmp(&a.0.len()));
    let mut args: AVec<'b, AString<'b>> = Vec::new_in(alloc);
    let mut exec_args: AVec<'b, AVec<'b, Segment<'b>>> = Vec::new_in(alloc);
    let mut files: AVec<'b, (AString<'b>, AString<'b>)> = Vec::new_in(alloc);
    let mut paths: AVec<'b, (AString<'b>, &'s str)> = Vec::new_in(alloc);
    let mut input_rels: AVec<'b, (AString<'b>, AString<'b>)> = Vec::new_in(alloc);
    let (mut inputs, mut outputs) = (0u32, 0u32);
    let mut prev_is_define_flag = false;
    for arg in spawn.args.iter().copied() {
        let is_define_val = prev_is_define_flag
            || (arg.starts_with("-D") && arg.len() > 2)
            || (arg.starts_with("-U") && arg.len() > 2);
        prev_is_define_flag = matches!(arg, "-D" | "-U");
        if is_define_val {
            // Macro definitions (`-D...`, `-D <val>`, `-U...`) carry literal C tokens/strings and
            // must NEVER have embedded root substrings rewritten to `$OUT_DIR` / `$SDKROOT`.
            args.push(own(arg));
            let mut segs: AVec<'b, Segment<'b>> = Vec::new_in(alloc);
            segs.push(Segment::Literal(own(arg)));
            exec_args.push(segs);
            continue;
        }
        let Some(rel_owned) = normalize_under_out_dir(arg, spawn.out_dir, alloc) else {
            args.push(spelled_roots(arg, &roots, alloc));
            exec_args.push(spelled_segments(arg, &roots, alloc));
            continue;
        };
        let rel = rel_owned.as_str();
        // One path, one placeholder, however often it is named.
        if let Some((spelled, _)) = paths.iter().find(|(_, actual)| *actual == arg) {
            args.push(own(spelled.as_str()));
            let mut segs: AVec<'b, Segment<'b>> = Vec::new_in(alloc);
            segs.push(Segment::Placeholder(own(spelled.as_str())));
            exec_args.push(segs);
            continue;
        }
        let digest = file(rel);
        let (head, count) = match digest {
            Some(_) => (INPUT, &mut inputs),
            None => (OUTPUT, &mut outputs),
        };
        let mut spelled = own(head);
        push_decimal(&mut spelled, *count);
        *count += 1;
        spelled.push_str(extension(rel));
        if let Some(digest) = digest {
            files.push((own(spelled.as_str()), hex(&digest, alloc)));
            input_rels.push((own(spelled.as_str()), own(rel)));
        }
        let mut segs: AVec<'b, Segment<'b>> = Vec::new_in(alloc);
        segs.push(Segment::Placeholder(own(spelled.as_str())));
        exec_args.push(segs);
        paths.push((own(spelled.as_str()), arg));
        args.push(spelled);
    }
    // Each placeholder once (above), so the unstable sort is the one order.
    files.sort_unstable_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    let mut env: AVec<'b, (AString<'b>, AString<'b>)> = Vec::new_in(alloc);
    let mut exec_env: AVec<'b, (AString<'b>, AVec<'b, Segment<'b>>)> = Vec::new_in(alloc);
    for name in CLANG_ENV {
        if let Some((_, value)) = spawn.env.iter().rev().find(|(have, _)| *have == name) {
            env.push((own(name), spelled_roots(value, &roots, alloc)));
            exec_env.push((own(name), spelled_segments(value, &roots, alloc)));
        }
    }
    let row = Row { clang: own(clang), program: own(name_of(spawn.program)), args, env, files, answer: Answer::Pending };
    Asked { row, exec_args, exec_env, paths, input_rels }
}

/// The extension of the last name of `path` with its dot (`.c`, `.cpp`), or "" where it has
/// none a placeholder may carry (no dot after the name's first byte, or a byte past
/// `[A-Za-z0-9+_-]`).
fn extension(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(dot) if dot > 0 && dot + 1 < name.len() && name[dot + 1..].bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'_' | b'-')) => &name[dot..],
        _ => "",
    }
}

fn push_decimal(out: &mut AString<'_>, value: u32) {
    let mut digits = [0u8; 10];
    let mut at = digits.len();
    let mut left = value;
    loop {
        at -= 1;
        digits[at] = b'0' + (left % 10) as u8;
        left /= 10;
        if left == 0 {
            break;
        }
    }
    for digit in &digits[at..] {
        out.push(char::from(*digit));
    }
}

/// Whether two rows have one key: everything but the answer, `env` and `files` as sets.
fn same_key(a: &Row<'_>, b: &Row<'_>) -> bool {
    let same_set = |x: &[(AString<'_>, AString<'_>)], y: &[(AString<'_>, AString<'_>)]| {
        x.len() == y.len() && x.iter().all(|(k, v)| y.iter().any(|(k2, v2)| k.as_str() == k2.as_str() && v.as_str() == v2.as_str()))
    };
    a.clang.as_str() == b.clang.as_str()
        && a.program.as_str() == b.program.as_str()
        && a.args.len() == b.args.len()
        && a.args.iter().zip(b.args.iter()).all(|(x, y)| x.as_str() == y.as_str())
        && same_set(&a.env, &b.env)
        && same_set(&a.files, &b.files)
}

/// Whether `prev` is a genuine path start boundary immediately before a substituted root directory
/// (`None` for start-of-value, `'='` for `--flag=/path` / `-f*-prefix-map=old=new`, or `':'` for
/// colon-delimited path lists). Prevents matching embedded substrings like `prefix/x/out/file`.
fn is_root_start_boundary(prev: Option<u8>) -> bool {
    matches!(prev, None | Some(b'=') | Some(b':'))
}

/// Whether `next` is a genuine path boundary immediately after a substituted root directory
/// (`None` for end-of-value, `'/'` for a child path component, or `'='` only inside
/// `-f*-prefix-map=old=new`). Never treats Unix filename punctuation or whitespace as delimiters.
fn is_root_end_boundary(next: Option<u8>, allow_eq: bool) -> bool {
    match next {
        None | Some(b'/') => true,
        Some(b'=') => allow_eq,
        _ => false,
    }
}

/// `text` split structurally into literal spans and real guest-folder placeholders.
fn spelled_segments<'b>(text: &str, roots: &[(&str, &str)], alloc: Alloc<'b>) -> AVec<'b, Segment<'b>> {
    let allow_eq = is_prefix_map_flag(text);
    let mut out: AVec<'b, Segment<'b>> = Vec::new_in(alloc);
    let mut lit = String::new_in(alloc);
    let bytes = text.as_bytes();
    let mut at = 0usize;
    'scan: while let Some(rest) = text.get(at..) {
        if rest.is_empty() {
            break;
        }
        let prev = at.checked_sub(1).and_then(|i| bytes.get(i).copied());
        if is_root_start_boundary(prev) {
            for (root, spelling) in roots.iter().copied() {
                if !root.is_empty()
                    && rest.starts_with(root)
                    && is_root_end_boundary(bytes.get(at + root.len()).copied(), allow_eq)
                {
                    if !lit.is_empty() {
                        out.push(Segment::Literal(lit));
                        lit = String::new_in(alloc);
                    }
                    let seg = match spelling {
                        OUT_DIR => Segment::OutDir,
                        SDKROOT => Segment::SdkRoot,
                        MOUNT => Segment::Mount,
                        other => Segment::Placeholder(String::from_str_in(other, alloc)),
                    };
                    out.push(seg);
                    at += root.len();
                    continue 'scan;
                }
            }
        }
        if let Some(ch) = rest.chars().next() {
            lit.push(ch);
            at += ch.len_utf8();
        } else {
            break;
        }
    }
    if !lit.is_empty() {
        out.push(Segment::Literal(lit));
    }
    out
}

/// `text` with each root (a guest folder) spelled as its placeholder where it stands whole.
fn spelled_roots<'b>(text: &str, roots: &[(&str, &str)], alloc: Alloc<'b>) -> AString<'b> {
    let allow_eq = is_prefix_map_flag(text);
    let mut out = String::new_in(alloc);
    let bytes = text.as_bytes();
    let mut at = 0;
    'scan: while at < text.len() {
        let prev = at.checked_sub(1).and_then(|i| bytes.get(i).copied());
        if is_root_start_boundary(prev) {
            for (root, spelling) in roots.iter().copied() {
                if !root.is_empty()
                    && text[at..].starts_with(root)
                    && is_root_end_boundary(bytes.get(at + root.len()).copied(), allow_eq)
                {
                    out.push_str(spelling);
                    at += root.len();
                    continue 'scan;
                }
            }
        }
        let width = text[at..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&text[at..at + width]);
        at += width;
    }
    out
}

/// `text` with every `from` of `pairs` put as its `to`, the first pair that matches at a place
/// winning.
fn replaced<'b>(text: &str, pairs: &[(&str, &str)], alloc: Alloc<'b>) -> AString<'b> {
    let mut out = String::new_in(alloc);
    let mut at = 0;
    'scan: while at < text.len() {
        for (from, to) in pairs.iter().copied() {
            if !from.is_empty() && text[at..].starts_with(from) {
                out.push_str(to);
                at += from.len();
                continue 'scan;
            }
        }
        let width = text[at..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&text[at..at + width]);
        at += width;
    }
    out
}

fn hex<'b>(bytes: &[u8], alloc: Alloc<'b>) -> AString<'b> {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::new_in(alloc);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    out
}

/// `text` as a JSON string, quoted.
fn push_json(out: &mut AString<'_>, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if u32::from(c) < 0x20 => {
                const DIGITS: &[u8; 16] = b"0123456789abcdef";
                let n = u32::from(c) as usize;
                out.push_str("\\u00");
                out.push(char::from(DIGITS[n >> 4]));
                out.push(char::from(DIGITS[n & 15]));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The row as one line of the table's JSON, its answer PENDING.
fn row_json<'b>(row: &Row<'_>, alloc: Alloc<'b>) -> AString<'b> {
    let mut out = String::new_in(alloc);
    out.push_str("{\"clang\": ");
    push_json(&mut out, row.clang.as_str());
    out.push_str(", \"program\": ");
    push_json(&mut out, row.program.as_str());
    out.push_str(", \"args\": [");
    for (at, arg) in row.args.iter().enumerate() {
        if at > 0 {
            out.push_str(", ");
        }
        push_json(&mut out, arg.as_str());
    }
    for (field, pairs) in [("env", &row.env), ("files", &row.files)] {
        out.push_str(if field == "env" { "], \"env\": {" } else { "}, \"files\": {" });
        for (at, (key, value)) in pairs.iter().enumerate() {
            if at > 0 {
                out.push_str(", ");
            }
            push_json(&mut out, key.as_str());
            out.push_str(": ");
            push_json(&mut out, value.as_str());
        }
    }
    out.push_str("}, \"answer\": \"PENDING\"}");
    out
}

/// The table's events, read with the offset of the last one kept for errors.
struct Parse<'t, 'a> {
    reader: Reader<'t, Alloc<'a>>,
    alloc: Alloc<'a>,
    at: usize,
}

impl<'t, 'a> Parse<'t, 'a> {
    fn error(&self, why: TableWhy) -> TableError {
        TableError { at: self.at, why }
    }

    fn shape(&self, wanted: &'static str) -> TableError {
        self.error(TableWhy::Shape(wanted))
    }

    fn next(&mut self) -> Result<Event<'t>, TableError> {
        match self.reader.next() {
            Ok(Some(item)) => {
                self.at = item.at;
                Ok(item.event)
            }
            Ok(None) => Err(self.shape("more text")),
            Err(e) => Err(TableError { at: e.at, why: TableWhy::Json(e.what) }),
        }
    }

    fn want(&mut self, event: Event<'t>, wanted: &'static str) -> Result<(), TableError> {
        if self.next()? == event { Ok(()) } else { Err(self.shape(wanted)) }
    }

    fn want_map(&mut self) -> Result<(), TableError> {
        self.want(Event::StartMap, "an object")
    }

    /// The next key of the open map, or `None` at its end. Keys are plain words.
    fn key(&mut self) -> Result<Option<&'t str>, TableError> {
        match self.next()? {
            Event::Key(key) => key.as_plain().map(Some).ok_or_else(|| self.shape("a key without escapes")),
            Event::EndMap => Ok(None),
            _ => Err(self.shape("a key")),
        }
    }

    fn own(&self, text: Str<'t>) -> AString<'a> {
        let mut out = String::new_in(self.alloc);
        text.decode_into(&mut out);
        out
    }

    fn text(&mut self) -> Result<AString<'a>, TableError> {
        match self.next()? {
            Event::Text(text) => Ok(self.own(text)),
            _ => Err(self.shape("a string")),
        }
    }

    /// An object of strings to strings, its keys once each.
    fn pairs(&mut self) -> Result<AVec<'a, (AString<'a>, AString<'a>)>, TableError> {
        self.want_map()?;
        let mut pairs: AVec<'a, (AString<'a>, AString<'a>)> = Vec::new_in(self.alloc);
        loop {
            let key = match self.next()? {
                Event::Key(key) => self.own(key),
                Event::EndMap => return Ok(pairs),
                _ => return Err(self.shape("a key")),
            };
            if pairs.iter().any(|(have, _)| have.as_str() == key.as_str()) {
                return Err(self.error(TableWhy::TwoKeys));
            }
            let value = self.text()?;
            pairs.push((key, value));
        }
    }

    /// The next row of the open list, or `None` at its end.
    fn row(&mut self) -> Result<Option<Row<'a>>, TableError> {
        match self.next()? {
            Event::StartMap => {}
            Event::EndList => return Ok(None),
            _ => return Err(self.shape("a row object")),
        }
        let (mut clang, mut program, mut args, mut env, mut files, mut answer) = (None, None, None, None, None, None);
        while let Some(key) = self.key()? {
            let fresh = match key {
                "clang" => clang.replace(self.text()?).is_none(),
                "program" => program.replace(self.text()?).is_none(),
                "args" => {
                    self.want(Event::StartList, "a list of arguments")?;
                    let mut list: AVec<'a, AString<'a>> = Vec::new_in(self.alloc);
                    loop {
                        match self.next()? {
                            Event::Text(text) => list.push(self.own(text)),
                            Event::EndList => break,
                            _ => return Err(self.shape("an argument string")),
                        }
                    }
                    args.replace(list).is_none()
                }
                "env" => env.replace(self.pairs()?).is_none(),
                "files" => files.replace(self.pairs()?).is_none(),
                "answer" => answer.replace(self.answer()?).is_none(),
                _ => return Err(self.shape("a row key of clang, program, args, env, files, answer")),
            };
            if !fresh {
                return Err(self.error(TableWhy::TwoKeys));
            }
        }
        match (clang, program, args, env, files, answer) {
            (Some(clang), Some(program), Some(args), Some(env), Some(files), Some(answer)) => {
                Ok(Some(Row { clang, program, args, env, files, answer }))
            }
            _ => Err(self.shape("a row with clang, program, args, env, files and answer")),
        }
    }

    /// `"PENDING"` or `{code, stdout, stderr}`.
    fn answer(&mut self) -> Result<Answer<'a>, TableError> {
        match self.next()? {
            Event::Text(text) if text.as_plain() == Some("PENDING") => return Ok(Answer::Pending),
            Event::StartMap => {}
            _ => return Err(self.shape("\"PENDING\" or {code, stdout, stderr}")),
        }
        let (mut code, mut stdout, mut stderr) = (None, None, None);
        while let Some(key) = self.key()? {
            let fresh = match key {
                "code" => match self.next()? {
                    Event::Int(n) => match n.as_i64().and_then(|n| i32::try_from(n).ok()) {
                        Some(n) => code.replace(n).is_none(),
                        None => return Err(self.shape("an exit status in i32")),
                    },
                    _ => return Err(self.shape("an exit status")),
                },
                "stdout" => stdout.replace(self.text()?).is_none(),
                "stderr" => stderr.replace(self.text()?).is_none(),
                _ => return Err(self.shape("an answer key of code, stdout, stderr")),
            };
            if !fresh {
                return Err(self.error(TableWhy::TwoKeys));
            }
        }
        match (code, stdout, stderr) {
            (Some(code), Some(stdout), Some(stderr)) => Ok(Answer::Known { code, stdout, stderr }),
            _ => Err(self.shape("an answer with code, stdout and stderr")),
        }
    }
}
