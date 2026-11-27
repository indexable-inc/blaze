//! Derived facts: spawns a build script makes whose true answer in the deterministic sandbox is
//! known from text blaze already holds, so they need no sandbox at all
//! (port27/hvbox-1116/READINESS.md 1f; recorded like D54's `git:` facts).
//!
//! - `pkg-config` (or `pkgconf`): the sandbox has no system library and no `.pc` file (v1), so
//!   every query naming modules (with or without version constraints) answers "not found", a
//!   query naming no module (including 0-arg `pkg-config`) exits 1 with pkg-config 0.29.2's
//!   `Must specify package names on the command line`, and `--version` answers the fixed string
//!   `0.29.2` (as D54 states `git version 2.51.0`).
//! - `sw_vers -productVersion` / `-buildVersion` / `-productName`: the declared userland's pins.
//! - A C compiler's identity probes (`--version`, `-dumpversion`, `-dumpfullversion`): the
//!   pinned clang's version text, and [`blaze_pins::RUSTC_LLVM`] for the dumped version.
//! - A C compiler's probes (`-E` family detection, `-?`, cc's flag checks): routed here and
//!   answered from pins/answers/clang.json by the caller ([`crate::c_tasks_probe`]).
//!
//! A form not handled here is refused by name ([`FactRefused`]), never guessed. A fact reads no
//! file: its record subject ([`subject`]) is its whole question.

#![blaze::sans_io]

use blaze_own::{AString, Alloc, String, Vec};

use crate::probe;

/// A spawn answered as a derived fact: its program's question, borrowed from the spawn.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fact<'s> {
    /// `pkg-config` or `pkgconf` with these arguments.
    PkgConfig { args: &'s [&'s str] },
    /// `sw_vers` with these arguments.
    SwVers { args: &'s [&'s str] },
    /// A C compiler's identity probe: `program` as spawned (a path or a name) and `arg`, one of
    /// `--version`, `-dumpversion`, `-dumpfullversion`.
    CompilerVersion { program: &'s str, arg: &'s str },
    /// A compiler probe ([`crate::c_tasks_probe::is_probe`]): answered from the pinned table
    /// pins/answers/clang.json by the caller (it needs `OUT_DIR`'s files and the spawn's
    /// environment), never from [`Pins`].
    CcProbe { program: &'s str, args: &'s [&'s str] },
}

/// The pinned values a fact is answered from; the caller passes them (the clang pin row and
/// the declared userland).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pins<'p> {
    /// What the pinned clang prints for `--version`, exactly (its trailing newline included).
    pub clang_version_text: &'p str,
    /// The declared userland's `sw_vers -productVersion` value, without a newline.
    pub macos_product_version: &'p str,
    /// The declared userland's `sw_vers -buildVersion` value, without a newline.
    pub macos_build_version: &'p str,
}

/// A fact's answer, as the spawned program would have exited and printed.
#[derive(Debug, PartialEq, Eq)]
pub struct FactAnswer<'a> {
    pub code: i32,
    pub stdout: AString<'a>,
    pub stderr: AString<'a>,
}

/// Why a fact is not answered: the script stops naming the spawn and this (rule: a form not
/// handled is refused by name, never guessed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FactRefused<'a> {
    /// A `pkg-config` argument the derived fact does not know (an unknown flag, a newline, an
    /// empty or whitespace-only argument, or a malformed version constraint), carried as given.
    PkgConfigArgs { arg: AString<'a> },
    /// `sw_vers` with arguments other than exactly one of `-productVersion`, `-buildVersion`,
    /// `-productName`: the arguments joined by spaces.
    SwVersArgs { args: AString<'a> },
    /// `sw_vers` asked on a host that declares no Apple userland (empty macOS version pins).
    NoAppleUserland,
    /// A compiler probe whose argument is not `--version`, `-dumpversion` or `-dumpfullversion`.
    CompilerArgs { program: AString<'a>, arg: AString<'a> },
    /// A compiler probe asked of [`answer`]: its answer is the table's (`c_tasks_probe`).
    TableProbe,
}

impl core::fmt::Display for FactRefused<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FactRefused::PkgConfigArgs { arg } => {
                write!(f, "pkg-config argument `{}` is not one the derived fact answers (READINESS 1f)", arg.as_str())
            }
            FactRefused::SwVersArgs { args } => {
                write!(f, "sw_vers `{}`: only one of -productVersion, -buildVersion, -productName is answered (READINESS 1f)", args.as_str())
            }
            FactRefused::NoAppleUserland => {
                f.write_str("sw_vers: the host declares no Apple userland, so sw_vers is not answered (READINESS 1f)")
            }
            FactRefused::CompilerArgs { program, arg } => {
                write!(f, "`{} {}`: only --version, -dumpversion, -dumpfullversion are answered for a compiler (READINESS 1f)", program.as_str(), arg.as_str())
            }
            FactRefused::TableProbe => f.write_str("a compiler probe is answered from pins/answers/clang.json (c_tasks_probe), not from the fact pins"),
        }
    }
}

/// The C compilers among [`probe::C_TOOLS`] whose identity probes print the pinned clang's
/// text: the other C tools (`ar`, `cmake`, `ld`, `nasm`, `xcrun`, ...) print their own and are
/// not facts.
const COMPILERS: &[&str] = &["c++", "cc", "clang", "clang++", "cpp", "g++", "gcc"];

/// The identity probes answered for a compiler.
const COMPILER_PROBES: &[&str] = &["--version", "-dumpversion", "-dumpfullversion"];

/// The `sw_vers` options answered.
const SW_VERS_OPTIONS: &[&str] = &["-productVersion", "-buildVersion", "-productName"];

/// What `pkg-config --version` answers: a stated fixed string (the version whose messages
/// [`not_found`] quotes).
pub const PKG_CONFIG_VERSION: &str = "0.29.2";

/// What `pkg-config` prints to stderr when no package name is given (main.c).
const NO_MODULES_STDERR: &str = "Must specify package names on the command line\n";

/// What `sw_vers -productName` prints on every macOS since 11.
const MACOS_PRODUCT_NAME: &str = "macOS";

/// The last path component of a spawned program.
fn name_of(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

/// Reads a spawn as a derived fact, or `None` when it is not one.
pub fn question<'s>(program: &'s str, args: &'s [&'s str]) -> Option<Fact<'s>> {
    let name = name_of(program);
    if matches!(name, "pkg-config" | "pkgconf") {
        return Some(Fact::PkgConfig { args });
    }
    if name == "sw_vers" {
        return match args {
            [one] if SW_VERS_OPTIONS.contains(one) => Some(Fact::SwVers { args }),
            _ => None,
        };
    }
    if crate::c_tasks_probe::is_probe(program, args) {
        return Some(Fact::CcProbe { program, args });
    }
    if probe::is_c_tool(program) && COMPILERS.contains(&name) {
        return match args {
            [one] if COMPILER_PROBES.contains(one) => Some(Fact::CompilerVersion { program, arg: *one }),
            _ => None,
        };
    }
    None
}

/// The six version comparison operators `pkg-config` 0.29.2 accepts (`parse.c`).
fn is_operator(word: &str) -> bool {
    matches!(word, "=" | "<" | "<=" | ">" | ">=" | "!=")
}

/// Whether `word` is a plain module name or version token (non-empty, not a flag, no whitespace,
/// no comparison-operator character, no comma).
fn is_module_or_version(word: &str) -> bool {
    !word.is_empty() && !word.starts_with('-') && !word.chars().any(|c| c.is_whitespace() || matches!(c, '<' | '>' | '=' | '!' | ','))
}

/// State of the `pkg-config` module-list grammar across arguments (`parse.c` `parse_module_list`):
/// `module [op version]` specs separated by whitespace or commas, in single-arg or split-arg form.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ModuleParse {
    /// Start of input, or after a flag; next token must be a module name.
    Ready,
    /// Just saw `,`; a module name must follow (no trailing or double comma).
    AfterComma,
    /// Just saw a module name; next may be an operator, `,`, or another module name.
    AfterModule,
    /// Just saw a comparison operator; a version token must follow.
    ExpectVersion,
    /// Just saw a version token; next may be `,` or another module name (never a second operator).
    AfterVersion,
}

/// pkg-config 0.29.2's message for a module it cannot find: pkg.c's `internal_get_package`
/// (the first three lines) and the `virtual:world` package's requirement check (the last).
fn not_found(module: &str, out: &mut AString<'_>) {
    out.push_str("Package ");
    out.push_str(module);
    out.push_str(" was not found in the pkg-config search path.\nPerhaps you should add the directory containing `");
    out.push_str(module);
    out.push_str(".pc'\nto the PKG_CONFIG_PATH environment variable\nPackage '");
    out.push_str(module);
    out.push_str("', required by 'virtual:world', not found\n");
}

/// `pkg-config` with `args`. The flags known: `--version`, `--exists`, `--libs`, `--cflags`,
/// `--modversion`, `--static`, `--print-errors`, none taking a value; any other is refused.
/// Positional arguments carry module names with optional version constraints (`=`, `<`, `<=`,
/// `>`, `>=`, `!=`), comma- or space-separated, in single-arg (`"glib-2.0 >= 2.56"`) or
/// split-arg (`"glib-2.0"`, `">="`, `"2.56"`) form. Newlines and malformed constraints are
/// refused by name.
fn pkg_config<'a>(args: &[&str], alloc: Alloc<'a>) -> Result<FactAnswer<'a>, FactRefused<'a>> {
    let (mut version, mut exists, mut output, mut print_errors) = (false, false, false, false);
    let mut modules = Vec::new_in(alloc);
    let mut state = ModuleParse::Ready;
    let mut last_arg = "";

    for &arg in args {
        if arg.contains(['\n', '\r']) {
            return Err(FactRefused::PkgConfigArgs { arg: String::from_str_in(arg, alloc) });
        }
        if arg.starts_with('-') {
            if matches!(state, ModuleParse::ExpectVersion | ModuleParse::AfterComma) {
                return Err(FactRefused::PkgConfigArgs { arg: String::from_str_in(arg, alloc) });
            }
            match arg {
                "--version" => version = true,
                "--exists" => exists = true,
                "--libs" | "--cflags" | "--modversion" => output = true,
                "--static" => {}
                "--print-errors" => print_errors = true,
                _ => return Err(FactRefused::PkgConfigArgs { arg: String::from_str_in(arg, alloc) }),
            }
            state = ModuleParse::Ready;
            continue;
        }

        last_arg = arg;
        let mut saw_token = false;
        for (part_index, comma_part) in arg.split(',').enumerate() {
            if part_index > 0 {
                saw_token = true;
                match state {
                    ModuleParse::AfterModule | ModuleParse::AfterVersion => state = ModuleParse::AfterComma,
                    ModuleParse::Ready | ModuleParse::AfterComma | ModuleParse::ExpectVersion => {
                        return Err(FactRefused::PkgConfigArgs { arg: String::from_str_in(arg, alloc) });
                    }
                }
            }
            for word in comma_part.split([' ', '\t']).filter(|w| !w.is_empty()) {
                saw_token = true;
                match state {
                    ModuleParse::Ready | ModuleParse::AfterComma | ModuleParse::AfterVersion => {
                        if !is_module_or_version(word) {
                            return Err(FactRefused::PkgConfigArgs { arg: String::from_str_in(arg, alloc) });
                        }
                        modules.push(word);
                        state = ModuleParse::AfterModule;
                    }
                    ModuleParse::AfterModule => {
                        if is_operator(word) {
                            state = ModuleParse::ExpectVersion;
                        } else if is_module_or_version(word) {
                            modules.push(word);
                            state = ModuleParse::AfterModule;
                        } else {
                            return Err(FactRefused::PkgConfigArgs { arg: String::from_str_in(arg, alloc) });
                        }
                    }
                    ModuleParse::ExpectVersion => {
                        if !is_module_or_version(word) {
                            return Err(FactRefused::PkgConfigArgs { arg: String::from_str_in(arg, alloc) });
                        }
                        state = ModuleParse::AfterVersion;
                    }
                }
            }
        }
        if !saw_token {
            return Err(FactRefused::PkgConfigArgs { arg: String::from_str_in(arg, alloc) });
        }
    }

    if matches!(state, ModuleParse::ExpectVersion | ModuleParse::AfterComma) {
        return Err(FactRefused::PkgConfigArgs { arg: String::from_str_in(last_arg, alloc) });
    }
    if version {
        // main.c answers `--version` before it reads any module.
        let mut stdout = String::from_str_in(PKG_CONFIG_VERSION, alloc);
        stdout.push('\n');
        return Ok(FactAnswer { code: 0, stdout, stderr: String::new_in(alloc) });
    }
    if modules.is_empty() {
        // main.c: with no package names (including 0-arg `pkg-config`), pkg-config always prints
        // this message to stderr (even under `--exists`) and exits 1.
        return Ok(FactAnswer { code: 1, stdout: String::new_in(alloc), stderr: String::from_str_in(NO_MODULES_STDERR, alloc) });
    }
    // main.c: errors are off by default for `--exists` and for no output option at all
    // (`--print-errors` turns them on), on for every output option.
    let loud = print_errors || (output && !exists);
    let mut stderr = String::new_in(alloc);
    if loud {
        for module in modules {
            not_found(module, &mut stderr);
        }
    }
    Ok(FactAnswer { code: 1, stdout: String::new_in(alloc), stderr })
}

fn joined<'a>(args: &[&str], by: char, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = String::new_in(alloc);
    for (at, arg) in args.iter().enumerate() {
        if at > 0 {
            out.push(by);
        }
        out.push_str(arg);
    }
    out
}

/// Answers a derived fact from the pins.
pub fn answer<'a>(fact: &Fact<'_>, pins: &Pins<'_>, alloc: Alloc<'a>) -> Result<FactAnswer<'a>, FactRefused<'a>> {
    match *fact {
        Fact::PkgConfig { args } => pkg_config(args, alloc),
        Fact::SwVers { args } => {
            if pins.macos_product_version.is_empty() || pins.macos_build_version.is_empty() {
                return Err(FactRefused::NoAppleUserland);
            }
            let value = match args {
                ["-productVersion"] => pins.macos_product_version,
                ["-buildVersion"] => pins.macos_build_version,
                ["-productName"] => MACOS_PRODUCT_NAME,
                _ => return Err(FactRefused::SwVersArgs { args: joined(args, ' ', alloc) }),
            };
            let mut stdout = String::from_str_in(value, alloc);
            stdout.push('\n');
            Ok(FactAnswer { code: 0, stdout, stderr: String::new_in(alloc) })
        }
        Fact::CompilerVersion { program, arg } => {
            let stdout = match arg {
                "--version" => String::from_str_in(pins.clang_version_text, alloc),
                "-dumpversion" | "-dumpfullversion" => {
                    let mut out = String::from_str_in(blaze_pins::RUSTC_LLVM, alloc);
                    out.push('\n');
                    out
                }
                _ => {
                    return Err(FactRefused::CompilerArgs {
                        program: String::from_str_in(program, alloc),
                        arg: String::from_str_in(arg, alloc),
                    });
                }
            };
            Ok(FactAnswer { code: 0, stdout, stderr: String::new_in(alloc) })
        }
        Fact::CcProbe { .. } => Err(FactRefused::TableProbe),
    }
}

/// The head of a per-probe compiler read subject:
/// `cc-probe\n<answerer 64-hex>\n<clang pin 64-hex>\n<probe key 64-hex>`.
pub const CC_PROBE_HEAD: &str = "cc-probe";

fn hex_nibble(n: u8) -> char {
    match n & 15 {
        d @ 0..=9 => char::from(b'0' + d),
        d => char::from(b'a' + (d - 10)),
    }
}

fn push_hex32(bytes: &[u8; 32], out: &mut AString<'_>) {
    for byte in bytes {
        out.push(hex_nibble(byte >> 4));
        out.push(hex_nibble(byte & 15));
    }
}

fn parse_hex32(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (i, slot) in bytes.iter_mut().enumerate() {
        *slot = u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(bytes)
}

/// Stable 32-byte digest of a probe's SDK / sysroot identity (empty string on Linux with no
/// sysroot, or the macOS SDK identity hex).
pub fn sdk_id_digest(sdk_id: &str) -> [u8; 32] {
    blaze_ids::ContentId::of("blaze c-probe sdk id 1", sdk_id.as_bytes()).0
}

/// Parsed fields of a `cc-probe\n<answerer 64-hex>\n<clang 64-hex>\n<sdk 64-hex>\n<key 64-hex>`
/// read subject.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CcProbeSubject {
    pub answerer: blaze_ids::ContentId,
    pub clang: [u8; 32],
    pub sdk: [u8; 32],
    pub key: crate::c_tasks_probe::ProbeKey,
}

/// The read subject for a resolved compiler probe, binding the active answerer strategy, pinned
/// clang archive SHA-256, active SDK identity digest, and canonical [`crate::c_tasks_probe::ProbeKey`].
pub fn cc_probe_subject<'a>(
    answerer: &blaze_ids::ContentId,
    clang: &[u8; 32],
    sdk: &[u8; 32],
    key: &crate::c_tasks_probe::ProbeKey,
    alloc: Alloc<'a>,
) -> AString<'a> {
    let mut out = String::from_str_in(CC_PROBE_HEAD, alloc);
    out.push('\n');
    push_hex32(&answerer.0, &mut out);
    out.push('\n');
    push_hex32(clang, &mut out);
    out.push('\n');
    push_hex32(sdk, &mut out);
    out.push('\n');
    push_hex32(&key.0.0, &mut out);
    out
}

/// Parses a `cc-probe\n<answerer 64-hex>\n<clang 64-hex>\n<sdk 64-hex>\n<key 64-hex>` read subject.
pub fn parse_cc_probe_subject(subject: &str) -> Option<CcProbeSubject> {
    let rest = subject.strip_prefix(CC_PROBE_HEAD)?.strip_prefix('\n')?;
    let mut parts = rest.split('\n');
    let answerer_hex = parts.next()?;
    let clang_hex = parts.next()?;
    let sdk_hex = parts.next()?;
    let key_hex = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    Some(CcProbeSubject {
        answerer: blaze_ids::ContentId(parse_hex32(answerer_hex)?),
        clang: parse_hex32(clang_hex)?,
        sdk: parse_hex32(sdk_hex)?,
        key: crate::c_tasks_probe::ProbeKey(blaze_ids::ContentId(parse_hex32(key_hex)?)),
    })
}

/// The fact's record subject for the script's read list (as D54's `git:` subjects): the whole
/// question, one field per line. `pkgconf` is recorded as `pkg-config`: both are answered alike.
pub fn subject<'a>(fact: &Fact<'_>, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = String::new_in(alloc);
    match *fact {
        Fact::PkgConfig { args } => {
            out.push_str("pkg-config");
            for arg in args {
                out.push('\n');
                out.push_str(arg);
            }
        }
        Fact::SwVers { args } => {
            out.push_str("sw_vers");
            for arg in args {
                out.push('\n');
                out.push_str(arg);
            }
        }
        Fact::CompilerVersion { program, arg } => {
            out.push_str("cc-version\n");
            out.push_str(name_of(program));
            out.push('\n');
            out.push_str(arg);
        }
        Fact::CcProbe { program, args } => {
            out.push_str("cc-probe\n");
            out.push_str(name_of(program));
            for arg in args {
                out.push('\n');
                out.push_str(arg);
            }
        }
    }
    out
}
