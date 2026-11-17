//! Tests of the derived facts (READINESS 1f, prompt W5), by their stated answers only; nothing
//! runs pkg-config, sw_vers or a compiler.
//!
//! pkg-config 0.29.2's source is the spec for its answers:
//! - pkg.c, `internal_get_package`, for a module with no `.pc` file:
//!   `Package %s was not found in the pkg-config search path.` /
//!   ``Perhaps you should add the directory containing `%s.pc'`` /
//!   `to the PKG_CONFIG_PATH environment variable`;
//! - pkg.c, the command line's modules being the requirements of the `virtual:world` package:
//!   `Package '%s', required by '%s', not found` with `virtual:world`;
//! - main.c: "Error printing is determined as follows: for --exists, --*-version, --list-all
//!   and no options at all, it's off by default and --print-errors will turn it on; for all
//!   other output options, it's on by default"; `--version` prints `VERSION` and exits 0 before
//!   any module is read; a module not found exits 1.
//!
//! Expected from `lanecheck.sh blaze-build-script test facts`:
//! `test blaze-build-script: exit 0; 8 passed, 0 failed`.

use blaze_own::Alloc;

use crate::facts::{Fact, FactAnswer, FactRefused, Pins, answer, question, subject};

const PINS: Pins<'static> = Pins {
    clang_version_text: "clang version 23.1.4\nTarget: aarch64-apple-darwin\nThread model: posix\nInstalledDir: /usr/bin\n",
    macos_product_version: "26.1",
    macos_build_version: "25B78",
};

/// The not-found message of pkg-config 0.29.2 for `module` (the module doc's quotes).
fn message(module: &str) -> blaze_own::AString<'static> {
    let mut out = blaze_own::String::new_in(Alloc::global());
    for piece in [
        "Package ",
        module,
        " was not found in the pkg-config search path.\nPerhaps you should add the directory containing `",
        module,
        ".pc'\nto the PKG_CONFIG_PATH environment variable\nPackage '",
        module,
        "', required by 'virtual:world', not found\n",
    ] {
        out.push_str(piece);
    }
    out
}

fn ask<'s>(program: &'s str, args: &'s [&'s str]) -> FactAnswer<'static> {
    let fact = question(program, args).expect("a derived fact");
    answer(&fact, &PINS, Alloc::global()).expect("answered")
}

/// libusb1-sys 0.7.0's build.rs, in order: `pkg-config --libs --cflags libusb-1.0` (with
/// `PKG_CONFIG_ALLOW_SYSTEM_*` set, which changes nothing when no `.pc` exists), then
/// `--modversion libusb-1.0`, then (the vendored path) `--libs --cflags libudev`. Each must
/// exit 1 with the message naming exactly that module, so the script takes the vendored path.
/// A fact that answered 0 (or named another module) fails here.
#[test]
fn facts_libusb_queries_are_not_found() {
    let queries: [(&[&str], &str); 3] = [
        (&["--libs", "--cflags", "libusb-1.0"], "libusb-1.0"),
        (&["--modversion", "libusb-1.0"], "libusb-1.0"),
        (&["--libs", "--cflags", "libudev"], "libudev"),
    ];
    for (args, module) in queries {
        let got = ask("/usr/bin/pkg-config", args);
        assert_eq!(got.code, 1, "{args:?}");
        assert_eq!(got.stdout, "", "{args:?}");
        assert_eq!(got.stderr, message(module), "{args:?}");
    }
}

/// Two modules: one message each, in the order named; `pkgconf` answers as `pkg-config`.
#[test]
fn facts_pkg_config_two_modules_in_order() {
    let got = ask("pkgconf", &["--static", "--libs", "zlib", "openssl"]);
    let mut want = message("zlib");
    want.push_str(message("openssl").as_str());
    assert_eq!((got.code, got.stderr), (1, want));
}

/// main.c: `--exists` is silent unless `--print-errors`; the code is 1 either way.
#[test]
fn facts_pkg_config_exists_is_silent_without_print_errors() {
    let silent = ask("pkg-config", &["--exists", "libusb-1.0"]);
    assert_eq!((silent.code, silent.stderr.as_str()), (1, ""));
    let loud = ask("pkg-config", &["--exists", "--print-errors", "libusb-1.0"]);
    assert_eq!((loud.code, loud.stderr), (1, message("libusb-1.0")));
}

/// `--version` is the stated fixed string, exit 0.
#[test]
fn facts_pkg_config_version() {
    let got = ask("pkg-config", &["--version"]);
    assert_eq!((got.code, got.stdout.as_str(), got.stderr.as_str()), (0, "0.29.2\n", ""));
}

/// An argument the fact does not know is refused naming it, never answered as "not found".
#[test]
fn facts_pkg_config_unknown_flag_is_refused_by_name() {
    let args: &[&str] = &["--frobnicate", "libusb-1.0"];
    let fact = question("pkg-config", args).expect("pkg-config is always a fact question");
    let refused = answer(&fact, &PINS, Alloc::global()).expect_err("refused");
    assert_eq!(refused, FactRefused::PkgConfigArgs { arg: blaze_own::String::from_str_in("--frobnicate", Alloc::global()) });
}

/// `sw_vers -productVersion` / `-buildVersion` print the pins, one line; two options are not a
/// fact (sw_vers takes one).
#[test]
fn facts_sw_vers_answers_the_pins() {
    assert_eq!(ask("/usr/bin/sw_vers", &["-productVersion"]).stdout, "26.1\n");
    assert_eq!(ask("sw_vers", &["-buildVersion"]).stdout, "25B78\n");
    assert_eq!(ask("sw_vers", &["-productName"]).stdout, "macOS\n");
    assert_eq!(question("sw_vers", &["-productVersion", "-buildVersion"]), None);
}

/// `clang --version` is the pin's text exactly; `cc -dumpversion` is the pinned LLVM's version;
/// a C tool that is not a compiler (`ar`, `cmake`) and a compile are not facts.
#[test]
fn facts_compiler_identity() {
    let got = ask("/usr/bin/clang", &["--version"]);
    assert_eq!((got.code, got.stdout.as_str()), (0, PINS.clang_version_text));
    assert_eq!(ask("cc", &["-dumpversion"]).stdout, "23.1.4\n");
    assert_eq!(question("cmake", &["--version"]), None);
    assert_eq!(question("ar", &["--version"]), None);
    assert_eq!(question("cc", &["-c", "x.c"]), None);
    assert_eq!(question("git", &["--version"]), None);
}

/// The record subjects: the whole question, one field per line, the program by name.
#[test]
fn facts_subjects() {
    let g = Alloc::global();
    let pkg: &[&str] = &["--modversion", "libusb-1.0"];
    assert_eq!(subject(&Fact::PkgConfig { args: pkg }, g), "pkg-config\n--modversion\nlibusb-1.0");
    assert_eq!(subject(&Fact::SwVers { args: &["-productVersion"] }, g), "sw_vers\n-productVersion");
    assert_eq!(subject(&Fact::CompilerVersion { program: "/usr/bin/clang", arg: "--version" }, g), "cc-version\nclang\n--version");
}
