//! What a unit's rustflags ask of its link, as rustc reads them: `-l`, `-L`, `-C link-arg`,
//! `-C link-args`, and the `-C` options rustc turns into its own link arguments (`-C rpath`,
//! `-C relro-level`, `-C link-dead-code`, `-C strip`, `-C linker`, `-C linker-flavor`,
//! `-C default-linker-libraries`, `-C link-self-contained`, `-C prefer-dynamic`), and
//! `-C target-feature`'s `crt-static`, which is the link's and not the code's.
//!
//! [`Rustflags::read`] types each; the code generator's own (`-C target-cpu`, ...) are
//! `hir_ty_codegen`'s toolchain's, which leaves these to the link. [`set`] takes or refuses each
//! `-C` option for the image being linked: none is dropped. A library and a directory are read
//! as a build script's `rustc-link-lib` and `rustc-link-search` are, by the caller, who knows
//! which crate's rustflags gave them.

use std::path::PathBuf;

use super::{ArgFrom, GivenArg};
use crate::{Debug, Format, Output, OutputKind};

/// What a unit's rustflags ask of its link, in the order given.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Rustflags {
    /// Each `-C link-arg=ARG`, and each word of a `-C link-args=ARGS` split on whitespace.
    pub args: Vec<GivenArg>,
    /// Each `-l [KIND[:MODIFIERS]=]NAME[:RENAME]`'s value, as a build script's
    /// `rustc-link-lib` gives it.
    pub libs: Vec<String>,
    /// Each `-L [KIND=]PATH`'s value, as a build script's `rustc-link-search` gives it.
    pub search: Vec<PathBuf>,
    /// Each `-C` option of the link's, as written and typed.
    pub options: Vec<GivenOption>,
}

/// A `-C` option of the link's and the flag as written (`-C strip=symbols`), which an error
/// refusing it names.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: tests compare them
pub struct GivenOption {
    pub flag: String,
    pub option: CodegenOption,
}

/// A `-C` option rustc turns into link arguments of its own, typed.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: tests compare them
pub enum CodegenOption {
    /// `-C rpath[=BOOL]`: an rpath to each Rust dylib the image loads, and on macOS a shared
    /// library's install name `@rpath/FILE`.
    Rpath(bool),
    /// `-C relro-level=full|partial|off|none`.
    RelroLevel(RelroLevel),
    /// `-C link-dead-code[=BOOL]`: no `--gc-sections` (`-dead_strip`).
    LinkDeadCode(bool),
    /// `-C strip=none|debuginfo|symbols`.
    Strip(Strip),
    /// `-C linker=PATH`: the program rustc runs to link.
    Linker(String),
    /// `-C linker-flavor=FLAVOR`: how rustc spells the link for that program.
    LinkerFlavor(String),
    /// `-C default-linker-libraries[=BOOL]`: without `-nodefaultlibs`.
    DefaultLinkerLibraries(bool),
    /// `-C link-self-contained[=BOOL]`: rustc's own copies of the C runtime's start files.
    LinkSelfContained(bool),
    /// `-C prefer-dynamic[=BOOL]`: the standard library as `libstd-*.so`.
    PreferDynamic(bool),
    /// `-C target-feature=+crt-static` (`true`) or `-crt-static`: the C runtime linked
    /// statically. The code generator's own features are its toolchain's.
    CrtStatic(bool),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RelroLevel {
    /// `-z relro -z now`.
    Full,
    /// `-z relro`, bound lazily.
    Partial,
    /// `-z norelro`.
    Off,
    /// Nothing given: the linker's default.
    None,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Strip {
    None,
    Debuginfo,
    Symbols,
}

impl Rustflags {
    /// What `rustflags` ask of the link, as rustc reads them (`-C`, `-C...`, `--codegen` and
    /// `--codegen=` alike, `_` in an option's name read as `-`; `-l` and `-L` with their value
    /// apart or joined). An option of the link's whose value rustc would not take is an error
    /// naming it; every other flag is not the link's, and is left to who reads it.
    pub fn read(rustflags: &[String]) -> Result<Rustflags, String> {
        // alloc: once per link, what its rustflags ask of it (empty for most)
        let mut out = Rustflags::default();
        let mut words = rustflags.iter().map(String::as_str);
        while let Some(word) = words.next() {
            let mut value_of = |flag: &str| {
                // alloc: an error
                words.next().ok_or_else(|| format!("`{flag}` ends the rustflags without its value"))
            };
            let (flag, value) = match word {
                "-C" | "--codegen" => ("-C", value_of(word)?),
                "-l" => ("-l", value_of(word)?),
                "-L" => ("-L", value_of(word)?),
                _ => match word.strip_prefix("--codegen=") {
                    Some(value) => ("-C", value),
                    None if word.starts_with("--") => continue,
                    None => match word.split_at_checked(2) {
                        Some((flag @ ("-C" | "-l" | "-L"), value)) => (flag, value),
                        _ => continue,
                    },
                },
            };
            match flag {
                "-l" => {
                    // alloc: per `-l` in the rustflags, its value the link owns
                    out.libs.push(value.to_owned());
                }
                // alloc: per `-L` in the rustflags, its value the link owns
                "-L" => out.search.push(PathBuf::from(value)),
                _ => out.codegen(value)?,
            }
        }
        Ok(out)
    }

    /// One `-C name[=value]`: the link's, or not this module's.
    fn codegen(&mut self, option: &str) -> Result<(), String> {
        let (name, value) = match option.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (option, None),
        };
        // alloc: per `-C` read, its name as rustc spells it (`_` is `-`)
        let name = name.replace('_', "-");
        let flag = || match value {
            // alloc: the flag as written, which an error names
            Some(value) => format!("-C {name}={value}"),
            // alloc: as above
            None => format!("-C {name}"),
        };
        let given = |arg: &str| GivenArg {
            // alloc: the link owns its arguments, once per argument per link
            arg: arg.to_owned(),
            from: ArgFrom::Rustflags,
        };
        let refuse = |why: &str| -> String {
            // alloc: an error
            format!("`{}` in the rustflags: {why}", flag())
        };
        let boolean = || match value {
            None | Some("y" | "yes" | "on" | "true") => Ok(true),
            Some("n" | "no" | "off" | "false") => Ok(false),
            Some(_) => Err(refuse("not `y`, `yes`, `on`, `true`, `n`, `no`, `off` or `false`")),
        };
        let named = |what: &str| match value {
            // alloc: the option owns its value
            Some(it) if !it.is_empty() => Ok(it.to_owned()),
            _ => Err(refuse(what)),
        };
        let option = match name.as_str() {
            "link-arg" => {
                self.args.push(given(value.unwrap_or_default()));
                return Ok(());
            }
            "link-args" => {
                self.args.extend(value.unwrap_or_default().split_whitespace().map(given));
                return Ok(());
            }
            "rpath" => CodegenOption::Rpath(boolean()?),
            "link-dead-code" => CodegenOption::LinkDeadCode(boolean()?),
            "default-linker-libraries" => CodegenOption::DefaultLinkerLibraries(boolean()?),
            "prefer-dynamic" => CodegenOption::PreferDynamic(boolean()?),
            "link-self-contained" => match boolean() {
                Ok(on) => CodegenOption::LinkSelfContained(on),
                Err(_) => {
                    return Err(refuse(
                        "blaze reads it as a yes or no, not the unstable components \
                         (`+linker`, `-crto`)",
                    ));
                }
            },
            "relro-level" => CodegenOption::RelroLevel(match value {
                Some("full") => RelroLevel::Full,
                Some("partial") => RelroLevel::Partial,
                Some("off") => RelroLevel::Off,
                Some("none") => RelroLevel::None,
                _ => return Err(refuse("not `full`, `partial`, `off` or `none`")),
            }),
            "strip" => CodegenOption::Strip(match value {
                Some("none") => Strip::None,
                Some("debuginfo") => Strip::Debuginfo,
                Some("symbols") => Strip::Symbols,
                _ => return Err(refuse("not `none`, `debuginfo` or `symbols`")),
            }),
            "target-feature" => {
                // The last `crt-static` named wins, as rustc reads it; the rest are the code's.
                let crt = value.unwrap_or_default().split(',').rev().find_map(|it| match it {
                    "+crt-static" => Some(true),
                    "-crt-static" => Some(false),
                    _ => None,
                });
                match crt {
                    Some(on) => CodegenOption::CrtStatic(on),
                    None => return Ok(()),
                }
            }
            "linker" => CodegenOption::Linker(named("no program named")?),
            "linker-flavor" => CodegenOption::LinkerFlavor(named("no flavor named")?),
            _ => return Ok(()),
        };
        self.options.push(GivenOption { flag: flag(), option });
        Ok(())
    }
}

/// Why each refused option is: the text the error carries after it.
mod why {
    pub(super) const RELRO: &str = "blaze's linker always links an ELF image with `-z relro` and \
                                    `-z now` (`full`), and has no way to undo either";
    pub(super) const RELRO_MACHO: &str = "rustc hands ld64 `-z relro`, which a Mach-O image has no \
                                          such thing as; blaze's linker does not read it";
    pub(super) const DEAD_CODE: &str = "blaze's linker always drops every section nothing \
                                        reachable refers to, and cannot keep dead code";
    pub(super) const STRIP_SYMBOLS: &str = "blaze's linker always writes the functions' local \
                                            symbols (for backtraces and debuggers), and cannot \
                                            leave them out yet";
    pub(super) const LINKER: &str = "blaze links with its own linker and runs no other; a program \
                                     other than a C driver or a system linker (a wrapper script, \
                                     `zig cc`) can change the link in ways blaze cannot see. \
                                     Remove it, or give what it adds as link arguments";
    pub(super) const FLAVOR: &str = "it spells the link for another linker than the one whose \
                                     spelling blaze reads this target's link arguments in";
    pub(super) const DEFAULT_LIBS: &str = "blaze links the libraries the crates name (`#[link]`, \
                                           `-l`) and the C runtime's start files, never the C \
                                           driver's default libraries";
    pub(super) const SELF_CONTAINED: &str = "blaze links the C runtime of the target's system, \
                                             never rustc's self-contained copies";
    pub(super) const CRT_STATIC: &str = "blaze links the C runtime dynamically, and a static C \
                                         runtime is not implemented";
    pub(super) const PREFER_DYNAMIC: &str = "blaze links the standard library and every Rust \
                                             crate into the image, and has no `libstd-*.so` to \
                                             load instead";
}

/// The program names blaze's linker stands in for: a C driver or a system linker, each of
/// which links what blaze's linker links. A version (`clang-18`) or a cross prefix
/// (`aarch64-linux-gnu-gcc`) is the same program.
const LINKERS: &[&str] = &[
    "cc", "gcc", "clang", "c++", "g++", "clang++", "ld", "ld.lld", "ld64.lld", "lld", "lld-link",
    "rust-lld", "ld.bfd", "ld.gold", "gold", "mold", "ld.mold", "ld64", "link", "wasm-ld",
];

/// Whether blaze's linker stands in for `-C linker=path` ([`LINKERS`]).
fn is_plain_linker(path: &str) -> bool {
    let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let file = file.strip_suffix(".exe").unwrap_or(file);
    // `clang-18`, `gcc-13.2`: a version after the last `-`.
    let file = match file.rsplit_once('-') {
        Some((name, version))
            if !version.is_empty() && version.bytes().all(|it| it.is_ascii_digit() || it == b'.') =>
        {
            name
        }
        _ => file,
    };
    LINKERS.iter().any(|name| {
        file == *name
            || file.strip_suffix(name).is_some_and(|prefix| prefix.len() > 1 && prefix.ends_with('-'))
    })
}

/// The flavors that spell the link as blaze reads `format`'s link arguments
/// ([`super::Spelling::of`]): through `cc` for ELF and Mach-O, `link.exe`'s and `wasm-ld`'s own.
fn flavors(format: Format) -> &'static [&'static str] {
    match format {
        Format::Elf => &["gcc", "gnu-cc", "gnu-lld-cc"],
        Format::MachO => &["gcc", "darwin-cc", "darwin-lld-cc"],
        Format::Pe => &["msvc", "msvc-lld", "lld-link"],
        Format::Wasm => &["wasm-ld", "wasm-lld"],
    }
}

/// What `option` asks of `output`, set on it; `Err` with why, when blaze's linker cannot do it.
/// Each taken without a change says why in its arm.
pub(super) fn set(output: &mut Output, option: &CodegenOption) -> Result<(), &'static str> {
    let elf = output.format == Format::Elf;
    match *option {
        // Off: rustc's default.
        CodegenOption::Rpath(false) => {}
        CodegenOption::Rpath(true) => {
            // rustc names a macOS shared library `@rpath/FILE` (its `-install_name`); the rpaths
            // it adds are to Rust dylibs, and blaze links every Rust crate into the image.
            if output.format == Format::MachO
                && let OutputKind::Shared { name, .. } = &mut output.kind
            {
                let file = name.rsplit('/').next().unwrap_or(name);
                // alloc: the install name the output owns
                *name = format!("@rpath/{file}");
            }
        }
        // What blaze's linker writes: `PT_GNU_RELRO` and `DF_BIND_NOW`.
        CodegenOption::RelroLevel(RelroLevel::Full) if elf => {}
        CodegenOption::RelroLevel(_) if elf => return Err(why::RELRO),
        // Nothing given: what rustc does for this format by default.
        CodegenOption::RelroLevel(RelroLevel::None) => {}
        CodegenOption::RelroLevel(_) if output.format == Format::MachO => {
            return Err(why::RELRO_MACHO);
        }
        // rustc's `link.exe` and `wasm-ld` linkers give nothing for it.
        CodegenOption::RelroLevel(_) => {}
        // Off: blaze's linker drops what nothing reachable refers to, as rustc's does.
        CodegenOption::LinkDeadCode(false) => {}
        CodegenOption::LinkDeadCode(true) => return Err(why::DEAD_CODE),
        // Nothing stripped: what blaze's linker writes.
        CodegenOption::Strip(Strip::None) => {}
        CodegenOption::Strip(Strip::Debuginfo) => output.debug = Debug::None,
        CodegenOption::Strip(Strip::Symbols) => return Err(why::STRIP_SYMBOLS),
        // A C driver or system linker: blaze's linker writes what it would.
        CodegenOption::Linker(ref path) if is_plain_linker(path) => {}
        CodegenOption::Linker(_) => return Err(why::LINKER),
        // The spelling blaze reads the link arguments in.
        CodegenOption::LinkerFlavor(ref flavor) if flavors(output.format).contains(&&**flavor) => {}
        CodegenOption::LinkerFlavor(_) => return Err(why::FLAVOR),
        // Off: rustc's `-nodefaultlibs`, which blaze's linker links as.
        CodegenOption::DefaultLinkerLibraries(false) => {}
        CodegenOption::DefaultLinkerLibraries(true) => return Err(why::DEFAULT_LIBS),
        // Off: the system's C runtime, which blaze links.
        CodegenOption::LinkSelfContained(false) => {}
        CodegenOption::LinkSelfContained(true) => return Err(why::SELF_CONTAINED),
        // Off: the standard library linked in, as blaze links it.
        CodegenOption::PreferDynamic(false) => {}
        CodegenOption::PreferDynamic(true) => return Err(why::PREFER_DYNAMIC),
        // The C runtime linked dynamically, as blaze links it.
        CodegenOption::CrtStatic(false) => {}
        CodegenOption::CrtStatic(true) => return Err(why::CRT_STATIC),
    }
    Ok(())
}
