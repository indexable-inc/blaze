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

use blaze_own::{AString, AVec, Alloc, String, Vec};

use crate::facts::{self, Pins};

/// The programs the shim answers as, by `argv[0]`'s last component. Sorted.
pub const PROGRAMS: [&str; 3] = ["pkg-config", "pkgconf", "sw_vers"];

/// The shim binary's file name.
pub const SHIM_NAME: &str = "blaze-fact-shim";

/// The variable naming the run's facts folder (the runner sets it in the script's
/// environment; children inherit it).
pub const FOLDER_VAR: &str = "BLAZE_FACT_SHIM_RUN";

/// Under the run's facts folder: the links, the pins file, the log folder and the log.
pub const BIN_DIR: &str = "bin";
pub const PINS_FILE: &str = "pins";
pub const LOG_DIR: &str = "log";
pub const LOG_FILE: &str = "asked";

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
    /// A log entry with no kind byte, no NUL end, or a subject not UTF-8.
    LogEntry { at: usize },
}

impl core::fmt::Display for ShimError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ShimError::PinsHead => write!(f, "the fact shim's {PINS_FILE} file has no `blaze fact pins` head (D112)"),
            ShimError::PinsField { field } => write!(f, "the fact shim's {PINS_FILE} file: field `{field}` is missing or malformed (D112)"),
            ShimError::PinsTrailing => write!(f, "the fact shim's {PINS_FILE} file has bytes after its last field (D112)"),
            ShimError::LogEntry { at } => write!(f, "the fact shim's {LOG_DIR}/{LOG_FILE}: entry at byte {at} is malformed (D112)"),
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
    /// pkg-config pkgconf sw_vers".
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
}
