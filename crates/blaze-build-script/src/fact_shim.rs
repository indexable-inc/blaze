//! D112 interim: the multi-call fact shim, the one seam by which a NATIVE build script's
//! machine probes (`sw_vers -productVersion`, `pkg-config --libs ..`) are answered from the
//! command's fact pins, as the wasm path answers them ([`crate::facts`]). Main's ruling for
//! brief N7 (2026-11-16): a probe is answered from the pins, never by granting the machine's
//! own tool; a question not in the table is refused by name; exec interposition under Seatbelt
//! is not wanted.
//!
//! The native runner (blaze-proc-macro `native_facts`) makes, per run, a folder holding
//! `bin/<program>` for each of [`PROGRAMS`] (links to the one shim binary, first on the
//! script's `PATH`), the [`PINS_FILE`] it writes from the command's pins ([`encode_pins`]),
//! and a writable `log/` where the shim appends each question ([`log_entry`]). The shim
//! (blaze-cli's `blaze-fact-shim` binary, its IO in blaze-proc-macro's `native_facts::shim_main`)
//! dispatches on its `argv[0]`, answers ONLY
//! from that pins file, and exits [`REFUSED_CODE`] naming the program and its arguments on
//! standard error for anything else: it never falls through to the machine's tool. The runner
//! reads the log back ([`log_entries`]) and files each question as one of the run's READS
//! (its subject [`crate::facts::subject`]'s form, its digest worked out again by blaze from
//! the pins), so the record's validity covers exactly the facts the script asked.
//!
//! The list of programs is the facts table's own: the programs [`crate::facts::question`]
//! answers a form of, but the C compilers (a native script runs the pinned clang by its
//! absolute path; a compiler name on `PATH` would shadow nothing the runner sets). `uname` and
//! `xcrun` have no row in the facts table today, so they are not linked: a script spawning
//! them gets the sandbox's refusal as before. A row added to facts.rs is linked by adding its
//! program here (the test below holds every name to a form `question` answers).
//!
//! INTERIM (D112): the shim exists only while native scripts run outside the deterministic
//! sandbox; every record of a native run stays local (marked `host-tool` or not, never offered
//! to a shared cache). Sans-IO: the binary and the runner do the IO.
#![blaze::sans_io]

use blaze_ids::ContentId;
use blaze_own::{AString, AVec, Alloc, String, Vec};

use crate::{
    facts::{self, Pins},
    probe::{self, Row, Table},
    probe_ask::{self, Ask, Verdict},
};

/// The programs the shim answers as, by `argv[0]`'s last component. Sorted.
pub const PROGRAMS: [&str; 5] = ["cargo", "pkg-config", "pkgconf", "rustc", "sw_vers"];

/// The shim binary's file name.
pub const SHIM_NAME: &str = "blaze-fact-shim";

/// The variable naming the run's facts folder (the runner sets it in the script's
/// environment; children inherit it).
pub const FOLDER_VAR: &str = "BLAZE_FACT_SHIM_RUN";

/// Under the run's facts folder: the links, the pins file, the probe table file, the log folder
/// and the log.
pub const BIN_DIR: &str = "bin";
pub const PINS_FILE: &str = "pins";
pub const PROBES_FILE: &str = "probes";
pub const LOG_DIR: &str = "log";
pub const LOG_FILE: &str = "asked";

/// Logged subject prefix for an answered `rustc` try-compile probe (keyed by the command's
/// `Table::id()` in `KeyInputs::answers`, identical to the Wasm runner).
pub const RUSTC_PROBE_PREFIX: &str = "rustc-probe\n";

/// The exit code of a refused question (a shell's "command not found", so a script that
/// checks the code sees no answer).
pub const REFUSED_CODE: i32 = 127;

const PINS_HEAD: &str = "blaze fact pins 1\n";
const NO_PINS: &str = "blaze fact pins none\n";
const FIELDS: [&str; 3] = ["clang_version_text", "macos_product_version", "macos_build_version"];

/// Why the pins file or the log cannot be read: each names the file's part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShimError {
    /// The pins file does not start with either head.
    PinsHead,
    /// A field of the pins file is missing, out of order, or its length is wrong.
    PinsField { field: &'static str },
    /// Bytes after the last field.
    PinsTrailing,
    /// The probe table file (`probes`) is malformed.
    Probes,
    /// A log entry with no kind byte, no NUL end, or a subject not UTF-8.
    LogEntry { at: usize },
}

impl core::fmt::Display for ShimError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ShimError::PinsHead => write!(f, "the fact shim's {PINS_FILE} file has no `blaze fact pins` head (D112)"),
            ShimError::PinsField { field } => write!(f, "the fact shim's {PINS_FILE} file: field `{field}` is missing or malformed (D112)"),
            ShimError::PinsTrailing => write!(f, "the fact shim's {PINS_FILE} file has bytes after its last field (D112)"),
            ShimError::Probes => write!(f, "the fact shim's {PROBES_FILE} file is malformed (D112)"),
            ShimError::LogEntry { at } => write!(f, "the fact shim's {LOG_DIR}/{LOG_FILE}: entry at byte {at} is malformed (D112)"),
        }
    }
}

const PROBES_HEAD: &[u8] = b"blaze fact probes 1\n";

/// Encodes `host` and `table` (the sorted keyed `rustc` probe table from `ScriptSetup`) for
/// `blaze-fact-shim` to answer `autocfg` and try-compile probes without host `rustc` (Rule 1).
pub fn encode_probes(host: &str, table: &Table<'_>, out: &mut AVec<'_, u8>) {
    out.extend_from_slice(PROBES_HEAD);
    out.extend_from_slice(host.as_bytes());
    out.push(b'\n');
    let count = u32::try_from(table.rows.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&count.to_le_bytes());
    for row in table.rows.iter().take(usize::try_from(count).unwrap_or(0)) {
        out.extend_from_slice(&row.key.0);
        out.push(u8::from(row.accepted));
    }
}

/// Decodes a `probes` file written by [`encode_probes`].
pub fn decode_probes<'b, 'a>(bytes: &'b [u8], alloc: Alloc<'a>) -> Result<(&'b str, AVec<'a, Row>), ShimError> {
    let rest = bytes.strip_prefix(PROBES_HEAD).ok_or(ShimError::Probes)?;
    let nl = rest.iter().position(|&b| b == b'\n').ok_or(ShimError::Probes)?;
    let host_bytes = rest.get(..nl).ok_or(ShimError::Probes)?;
    let host = core::str::from_utf8(host_bytes).map_err(|_| ShimError::Probes)?;
    let after_host = rest.get(nl + 1..).ok_or(ShimError::Probes)?;
    let count_bytes: [u8; 4] = after_host.get(..4).and_then(|s| s.try_into().ok()).ok_or(ShimError::Probes)?;
    let count = usize::try_from(u32::from_le_bytes(count_bytes)).map_err(|_| ShimError::Probes)?;
    let body = after_host.get(4..).ok_or(ShimError::Probes)?;
    let expected = count.checked_mul(33).ok_or(ShimError::Probes)?;
    if body.len() != expected {
        return Err(ShimError::Probes);
    }
    let mut rows: AVec<'a, Row> = Vec::with_capacity_in(count, alloc);
    for chunk in body.chunks_exact(33) {
        let key: [u8; 32] = chunk.get(..32).and_then(|s| s.try_into().ok()).ok_or(ShimError::Probes)?;
        let accepted = match chunk.get(32).copied() {
            Some(0) => false,
            Some(1) => true,
            _ => return Err(ShimError::Probes),
        };
        rows.push(Row { key: ContentId(key), accepted });
    }
    Ok((host, rows))
}

fn push_hex32(bytes: &[u8; 32], out: &mut AString<'_>) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for &byte in bytes {
        if let Some(&hi) = HEX.get(usize::from(byte >> 4)) {
            out.push(char::from(hi));
        }
        if let Some(&lo) = HEX.get(usize::from(byte & 0x0f)) {
            out.push(char::from(lo));
        }
    }
}

/// Answers a `rustc` probe (`autocfg` / try-compile / version) using the exact same keyed probe
/// machinery ([`probe_ask::classify`] + [`probe_ask::answer`]) as the Wasm runner (Rule 1, Rule 30).
/// Refuses unknown flags, `--print`, and unanswered probes loudly with [`REFUSED_CODE`] (Rule 3).
pub fn ask_rustc_probe<'a>(
    args: &[&str],
    snippet: &[u8],
    host: &str,
    table: &Table<'_>,
    alloc: Alloc<'a>,
) -> Reply<'a> {
    use core::fmt::Write as _;
    let mut raw_subject = String::from_str_in("rustc", alloc);
    for arg in args {
        raw_subject.push('\n');
        raw_subject.push_str(arg);
    }
    let refuse = |why: core::fmt::Arguments<'_>, subject: AString<'a>| -> Reply<'a> {
        let mut stderr = String::new_in(alloc);
        let _ = write!(stderr, "{SHIM_NAME}: `rustc");
        for arg in args {
            stderr.push(' ');
            stderr.push_str(arg);
        }
        let _ = writeln!(stderr, "` refused (D112 interim, rule 3): {why}");
        Reply { code: REFUSED_CODE, stdout: String::new_in(alloc), stderr, answered: false, subject }
    };
    match probe_ask::classify(args, alloc) {
        Err(refused) => refuse(format_args!("argument {}: {}", refused.at, refused.why), raw_subject),
        Ok(Ask::Version { verbose }) => {
            let subject = facts::subject(&facts::Fact::RustcVersion { verbose }, alloc);
            Reply { code: 0, stdout: probe::version(verbose, host, alloc), stderr: String::new_in(alloc), answered: true, subject }
        }
        Ok(Ask::Print { what, .. }) => refuse(format_args!("`--print {what}` is not answered by the keyed probe table"), raw_subject),
        Ok(Ask::Compile(compile)) => {
            let answered = probe_ask::answer(&compile, host, snippet, table);
            let mut key_hex = String::new_in(alloc);
            push_hex32(&answered.key.0, &mut key_hex);
            let mut probe_subject = String::from_str_in(RUSTC_PROBE_PREFIX, alloc);
            probe_subject.push_str(key_hex.as_str());
            match answered.verdict {
                Verdict::Accepted(_) => Reply {
                    code: 0,
                    stdout: String::new_in(alloc),
                    stderr: String::new_in(alloc),
                    answered: true,
                    subject: probe_subject,
                },
                Verdict::Rejected(rejection) => Reply {
                    code: 1,
                    stdout: String::new_in(alloc),
                    stderr: rejection.stderr(&compile, snippet, alloc),
                    answered: true,
                    subject: probe_subject,
                },
                Verdict::Unanswered(open) => refuse(
                    format_args!(
                        "try-compile probe {} for `{}` is not answered by the pinned probe table ({open:?})",
                        key_hex.as_str(),
                        answered.target
                    ),
                    raw_subject,
                ),
            }
        }
    }
}

/// The pins file's text: [`NO_PINS`] when the command has none (each question is then refused
/// by name), else the head and each field as `<name> <byte length>\n<value>\n`.
pub fn encode_pins(pins: Option<&Pins<'_>>, out: &mut AString<'_>) {
    let Some(pins) = pins else {
        out.push_str(NO_PINS);
        return;
    };
    out.push_str(PINS_HEAD);
    let values = [pins.clang_version_text, pins.macos_product_version, pins.macos_build_version];
    for (name, value) in FIELDS.iter().zip(values) {
        out.push_str(name);
        out.push(' ');
        push_decimal(value.len(), out);
        out.push('\n');
        out.push_str(value);
        out.push('\n');
    }
}

fn push_decimal(mut n: usize, out: &mut AString<'_>) {
    let mut digits = [0u8; 20];
    let mut at = digits.len();
    loop {
        at -= 1;
        digits[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    for d in &digits[at..] {
        out.push(*d as char);
    }
}

/// The pins a pins file holds; `None` for the no-pins file.
pub fn decode_pins(text: &str) -> Result<Option<Pins<'_>>, ShimError> {
    if text == NO_PINS {
        return Ok(None);
    }
    let mut rest = text.strip_prefix(PINS_HEAD).ok_or(ShimError::PinsHead)?;
    let mut values: [&str; 3] = [""; 3];
    for (slot, field) in values.iter_mut().zip(FIELDS) {
        let bad = ShimError::PinsField { field };
        let (line, after) = rest.split_once('\n').ok_or(bad)?;
        let (name, len) = line.split_once(' ').ok_or(bad)?;
        if name != field {
            return Err(bad);
        }
        let len: usize = len.parse().map_err(|_| bad)?;
        *slot = after.get(..len).ok_or(bad)?;
        rest = after.get(len..).and_then(|r| r.strip_prefix('\n')).ok_or(bad)?;
    }
    if !rest.is_empty() {
        return Err(ShimError::PinsTrailing);
    }
    Ok(Some(Pins { clang_version_text: values[0], macos_product_version: values[1], macos_build_version: values[2] }))
}

/// What the shim prints and exits with for one question, and the log's subject.
#[derive(Debug, PartialEq, Eq)]
pub struct Reply<'a> {
    pub code: i32,
    pub stdout: AString<'a>,
    pub stderr: AString<'a>,
    /// Whether it was answered from the pins (else refused with [`REFUSED_CODE`]).
    pub answered: bool,
    /// The question in [`facts::subject`]'s form (a form `question` does not read: the
    /// program's name, then one argument per line).
    pub subject: AString<'a>,
}

/// The program's name as the shim dispatches on it: `argv[0]`'s last component.
pub fn program_name(argv0: &str) -> &str {
    argv0.rsplit('/').next().unwrap_or(argv0)
}

/// The shim's answer to `argv0 args..` from `pins` (the run's pins file, decoded).
pub fn ask<'a>(argv0: &str, args: &[&str], pins: Option<&Pins<'_>>, alloc: Alloc<'a>) -> Reply<'a> {
    let name = program_name(argv0);
    let question = if PROGRAMS.contains(&name) { facts::question(name, args) } else { None };
    let subject = match &question {
        Some(fact) => facts::subject(fact, alloc),
        None => {
            let mut s = String::from_str_in(name, alloc);
            for arg in args {
                s.push('\n');
                s.push_str(arg);
            }
            s
        }
    };
    let refuse = |why: core::fmt::Arguments<'_>, subject: AString<'a>| -> Reply<'a> {
        use core::fmt::Write;
        let mut stderr = String::new_in(alloc);
        let _ = write!(stderr, "{SHIM_NAME}: `{name}");
        for arg in args {
            stderr.push(' ');
            stderr.push_str(arg);
        }
        let _ = writeln!(stderr, "` refused (D112 interim, rule 3): {why}");
        Reply { code: REFUSED_CODE, stdout: String::new_in(alloc), stderr, answered: false, subject }
    };
    let Some(fact) = question else {
        if !PROGRAMS.contains(&name) {
            return refuse(format_args!("`{name}` is not a program the fact shim answers (fact_shim::PROGRAMS)"), subject);
        }
        return refuse(format_args!("not a question the pinned facts table answers (blaze_build_script::facts)"), subject);
    };
    let Some(pins) = pins else {
        return refuse(format_args!("no pinned fact: the command gave no fact pins (brief P)"), subject);
    };
    match facts::answer(&fact, pins, alloc) {
        Ok(answer) => Reply { code: answer.code, stdout: answer.stdout, stderr: answer.stderr, answered: true, subject },
        Err(refused) => refuse(format_args!("{refused}"), subject),
    }
}

/// One log entry: `a` (answered) or `r` (refused), the subject, a NUL (an argument holds none).
pub fn log_entry(reply: &Reply<'_>, out: &mut AVec<'_, u8>) {
    out.push(if reply.answered { b'a' } else { b'r' });
    out.extend_from_slice(reply.subject.as_bytes());
    out.push(0);
}

/// The log's entries in the order written: (answered, subject).
pub fn log_entries<'l, 'a>(log: &'l [u8], alloc: Alloc<'a>) -> Result<AVec<'a, (bool, &'l str)>, ShimError> {
    let mut out = Vec::new_in(alloc);
    let mut at = 0;
    while at < log.len() {
        let bad = ShimError::LogEntry { at };
        let answered = match log[at] {
            b'a' => true,
            b'r' => false,
            _ => return Err(bad),
        };
        let end = log[at + 1..].iter().position(|b| *b == 0).ok_or(bad)? + at + 1;
        let subject = core::str::from_utf8(&log[at + 1..end]).map_err(|_| bad)?;
        out.push((answered, subject));
        at = end + 1;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use blaze_own::{String, Vec};

    use super::{PROGRAMS, REFUSED_CODE, ShimError, ask, decode_pins, encode_pins, log_entries, log_entry};
    use crate::facts::{Pins, question};

    const PINS: Pins<'static> = Pins { clang_version_text: "clang version 23.1.4\nTarget: arm64-apple-darwin\n", macos_product_version: "27.0.1", macos_build_version: "26A434" };

    /// The pins file round-trips (the clang text holds newlines), the no-pins file reads as
    /// none, and a damaged file is refused naming its field. Prints "pins file: round trip".
    #[test]
    fn the_pins_file_round_trips() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut text = String::new_in(alloc);
        encode_pins(Some(&PINS), &mut text);
        assert_eq!(decode_pins(text.as_str()), Ok(Some(PINS)));
        let mut none = String::new_in(alloc);
        encode_pins(None, &mut none);
        assert_eq!(decode_pins(none.as_str()), Ok(None));
        let cut = &text.as_str()[..text.len() - 3];
        assert_eq!(decode_pins(cut), Err(ShimError::PinsField { field: "macos_build_version" }));
        println!("pins file: round trip");
    }

    /// N7: `sw_vers -productVersion` answers the pinned version; with no pins it is refused
    /// with 127 naming the program and its argument; a form the table lacks is refused by name;
    /// the log keeps both kinds with their subjects. Prints "fact shim: sw_vers answered 27.0.1;
    /// no pins: 127 by name".
    #[test]
    fn sw_vers_is_answered_from_the_pins_and_refused_without_them() {
        let alloc = blaze_own::testing::heap().alloc();
        let reply = ask("/run/bin/sw_vers", &["-productVersion"], Some(&PINS), alloc);
        assert_eq!((reply.code, reply.stdout.as_str(), reply.answered), (0, "27.0.1\n", true));
        assert_eq!(reply.subject.as_str(), "sw_vers\n-productVersion");
        let refused = ask("sw_vers", &["-productVersion"], None, alloc);
        assert_eq!((refused.code, refused.answered, refused.stdout.as_str()), (REFUSED_CODE, false, ""));
        assert!(refused.stderr.as_str().contains("`sw_vers -productVersion` refused"), "{}", refused.stderr.as_str());
        let odd = ask("sw_vers", &["-productVersion", "-buildVersion"], Some(&PINS), alloc);
        assert_eq!(odd.code, REFUSED_CODE);
        let uname = ask("uname", &["-r"], Some(&PINS), alloc);
        assert!(uname.stderr.as_str().contains("not a program the fact shim answers"), "{}", uname.stderr.as_str());

        let mut log = Vec::new_in(alloc);
        log_entry(&reply, &mut log);
        log_entry(&refused, &mut log);
        let entries = log_entries(&log, alloc).expect("a whole log");
        assert_eq!(&*entries, &[(true, "sw_vers\n-productVersion"), (false, "sw_vers\n-productVersion")]);
        assert_eq!(log_entries(b"xsw_vers\0", alloc).err(), Some(ShimError::LogEntry { at: 0 }));
        println!("fact shim: sw_vers answered 27.0.1; no pins: 127 by name");
    }

    /// The shim's programs are the facts table's own: each is read as a question by
    /// `facts::question` in some form, and no C compiler is among them. Prints "shim programs:
    /// cargo pkg-config pkgconf rustc sw_vers".
    #[test]
    fn every_shim_program_is_a_facts_row() {
        assert!(PROGRAMS.is_sorted());
        for program in PROGRAMS {
            let asked = question(program, &["--version"]).or(question(program, &["-productVersion"]));
            assert!(asked.is_some(), "{program}: no form of it is a pinned fact");
        }
        for compiler in ["cc", "clang", "gcc"] {
            assert!(!PROGRAMS.contains(&compiler));
        }
        println!("shim programs: {}", PROGRAMS.join(" "));
    }

    /// Rule 1 (`no-rustc`): `blaze-fact-shim` answers `rustc -V` / `--version` / `-vV` and
    /// `cargo -V` / `--version` strictly from the pinned toolchain constants (never host `rustc`),
    /// and refuses try-compiles or unknown invocations with exit 127 naming them.
    #[test]
    fn rustc_and_cargo_version_probes_are_answered_and_compiles_refused() {
        let alloc = blaze_own::testing::heap().alloc();
        let linux_pins = Pins {
            clang_version_text: "clang version 23.1.4\nTarget: aarch64-unknown-linux-gnu\nThread model: posix\nInstalledDir: /usr/bin\n",
            macos_product_version: "",
            macos_build_version: "",
        };

        // Short `rustc -V` and `rustc --version` match `probe::version(false, ...)`.
        let expected_short = crate::probe::version(false, "", alloc);
        for args in [&["-V"][..], &["--version"][..]] {
            let reply = ask("/run/bin/rustc", args, Some(&linux_pins), alloc);
            assert_eq!((reply.code, reply.answered, reply.stderr.as_str()), (0, true, ""), "{args:?}");
            assert_eq!(reply.stdout.as_str(), expected_short.as_str(), "{args:?}");
            assert_eq!(reply.subject.as_str(), "rustc\n--version");
        }

        // Verbose `rustc -vV` and `--version --verbose` include the host triple from the pins.
        let expected_linux_v = crate::probe::version(true, "aarch64-unknown-linux-gnu", alloc);
        for args in [&["-vV"][..], &["--version", "--verbose"][..]] {
            let reply = ask("rustc", args, Some(&linux_pins), alloc);
            assert_eq!((reply.code, reply.answered, reply.stderr.as_str()), (0, true, ""), "{args:?}");
            assert_eq!(reply.stdout.as_str(), expected_linux_v.as_str(), "{args:?}");
            assert_eq!(reply.subject.as_str(), "rustc\n--version\n--verbose");
        }
        let mac_v = ask("rustc", &["-vV"], Some(&PINS), alloc);
        assert_eq!(mac_v.code, 0);
        assert!(mac_v.stdout.as_str().contains("host: aarch64-apple-darwin\n"), "{}", mac_v.stdout.as_str());

        // `cargo -V`, `--version`, and `version` match `probe::cargo_version`.
        let expected_cargo = crate::probe::cargo_version(alloc);
        for args in [&["-V"][..], &["--version"][..], &["version"][..]] {
            let reply = ask("/run/bin/cargo", args, Some(&linux_pins), alloc);
            assert_eq!((reply.code, reply.answered, reply.stderr.as_str()), (0, true, ""), "{args:?}");
            assert_eq!(reply.stdout.as_str(), expected_cargo.as_str(), "{args:?}");
            assert_eq!(reply.subject.as_str(), "cargo\n--version");
        }

        // Try-compile and unknown invocations MUST be refused with exit 127 naming them.
        let compile = ask("rustc", &["--crate-type", "lib", "foo.rs"], Some(&linux_pins), alloc);
        assert_eq!((compile.code, compile.answered, compile.stdout.as_str()), (REFUSED_CODE, false, ""));
        assert!(
            compile.stderr.as_str().contains("`rustc --crate-type lib foo.rs` refused"),
            "{}",
            compile.stderr.as_str()
        );
        let stdin_compile = ask("rustc", &["--crate-type=rlib", "-"], Some(&linux_pins), alloc);
        assert_eq!((stdin_compile.code, stdin_compile.answered), (REFUSED_CODE, false));
        let print_cfg = ask("rustc", &["--print", "cfg"], Some(&linux_pins), alloc);
        assert_eq!((print_cfg.code, print_cfg.answered), (REFUSED_CODE, false));
        let cargo_build = ask("cargo", &["build"], Some(&linux_pins), alloc);
        assert_eq!((cargo_build.code, cargo_build.answered), (REFUSED_CODE, false));
        assert!(cargo_build.stderr.as_str().contains("`cargo build` refused"), "{}", cargo_build.stderr.as_str());
    }

    /// `encode_probes`/`decode_probes` round-trip and `ask_rustc_probe` answers `autocfg`-style
    /// try-compiles via `probe_ask::classify` + `probe_ask::answer` (zero host `rustc`, Rule 1).
    #[test]
    fn rustc_probe_table_round_trip_and_autocfg_answers() {
        const HOST: &str = "aarch64-unknown-linux-gnu";
        let alloc = blaze_own::testing::heap().alloc();

        let mut rows = crate::probe::pinned_rows(HOST, crate::pinned::TEMPO, alloc);
        let ships = crate::probe_ask::ships_rows(HOST, ["core", "std"], alloc);
        rows.extend_from_slice(ships.as_slice());
        rows.sort_by(|a, b| a.key.cmp(&b.key));
        let table = crate::probe::Table { rows: rows.as_slice() };

        // Round-trip probe table encoding/decoding (and reject truncated/corrupt payloads).
        let mut encoded = Vec::new_in(alloc);
        super::encode_probes(HOST, &table, &mut encoded);
        let (decoded_host, decoded_rows) = super::decode_probes(encoded.as_slice(), alloc).expect("decode probes");
        assert_eq!(decoded_host, HOST);
        assert_eq!(decoded_rows.len(), table.rows.len());
        assert_eq!(super::decode_probes(&encoded.as_slice()[..encoded.len() - 1], alloc).err(), Some(ShimError::Probes));

        let decoded_table = crate::probe::Table { rows: decoded_rows.as_slice() };

        // 1. autocfg empty `#![no_std]` probe -> decided Accepted (exit 0) with `rustc-probe\n` subject.
        let autocfg_empty = super::ask_rustc_probe(
            &["--crate-type=lib", "--emit=llvm-ir", "--crate-name=probe0", "-"],
            b"#![no_std]\n",
            HOST,
            &decoded_table,
            alloc,
        );
        assert_eq!((autocfg_empty.code, autocfg_empty.answered, autocfg_empty.stderr.as_str()), (0, true, ""));
        assert!(autocfg_empty.subject.as_str().starts_with(super::RUSTC_PROBE_PREFIX), "{}", autocfg_empty.subject.as_str());

        // 2. Pinned autocfg probe (`f64::total_cmp` from `pinned::TEMPO`) -> answered from table (exit 0).
        let num_traits = super::ask_rustc_probe(
            &["--crate-type=lib", "--emit=llvm-ir", "--crate-name=probe1", "-"],
            b"pub fn probe() { let _ = 1f64.total_cmp(&2f64); }",
            HOST,
            &decoded_table,
            alloc,
        );
        assert_eq!((num_traits.code, num_traits.answered), (0, true));

        // 3. Removed language feature -> decided Rejected (exit 1, E0557 on stderr, answered = true).
        let bad_feat = super::ask_rustc_probe(
            &["--crate-type=lib", "--emit=metadata", "-"],
            b"#![feature(no_debug)]\n",
            HOST,
            &decoded_table,
            alloc,
        );
        assert_eq!((bad_feat.code, bad_feat.answered), (1, true));
        assert!(bad_feat.stderr.as_str().contains("E0557"), "{}", bad_feat.stderr.as_str());

        // 4. Unanswered try-compile and `--print cfg` are refused loudly with exit 127.
        let unknown = super::ask_rustc_probe(
            &["--crate-type=lib", "--emit=llvm-ir", "-"],
            b"pub fn definitely_unknown_probe_item_xyz() {}",
            HOST,
            &decoded_table,
            alloc,
        );
        assert_eq!((unknown.code, unknown.answered), (REFUSED_CODE, false));
        assert!(unknown.stderr.as_str().contains("not answered by the pinned probe table"), "{}", unknown.stderr.as_str());

        let print_cfg = super::ask_rustc_probe(&["--print", "cfg"], b"", HOST, &decoded_table, alloc);
        assert_eq!((print_cfg.code, print_cfg.answered), (REFUSED_CODE, false));
        assert!(print_cfg.stderr.as_str().contains("--print cfg"), "{}", print_cfg.stderr.as_str());
    }
}
