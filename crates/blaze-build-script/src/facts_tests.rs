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
//!   any module is read; no package names prints `Must specify package names on the command line`
//!   and exits 1 (even with `--exists`); a module not found exits 1.
//! - parse.c, `parse_module_list`: comparison operators are `=`, `<`, `<=`, `>`, `>=`, `!=`;
//!   modules are separated by whitespace or commas, in single-arg or split-arg form, and repeated
//!   modules are not deduplicated.
//!
//! Expected from `lanecheck.sh blaze-build-script test facts`:
//! `test blaze-build-script: exit 0; 15 passed, 0 failed`.

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

fn refuse(args: &[&str]) -> FactRefused<'static> {
    let fact = question("pkg-config", args).expect("pkg-config is always a fact question");
    answer(&fact, &PINS, Alloc::global()).expect_err("refused")
}

/// 0-arg `pkg-config` (as `openssl-sys` spawns to probe availability) and flag-only queries with
/// no package names always print `Must specify package names on the command line\n` and exit 1,
/// even when `--exists` is passed (main.c).
#[test]
fn facts_pkg_config_no_modules_prints_must_specify() {
    const WANT: &str = "Must specify package names on the command line\n";
    let queries: [&[&str]; 7] = [
        &[],
        &["--libs", "--cflags"],
        &["--exists"],
        &["--exists", "--print-errors"],
        &["--print-errors"],
        &["--static"],
        &["--modversion"],
    ];
    for args in queries {
        let got = ask("pkg-config", args);
        assert_eq!((got.code, got.stdout.as_str(), got.stderr.as_str()), (1, "", WANT), "{args:?}");
    }
    let pkgconf = ask("pkgconf", &[]);
    assert_eq!((pkgconf.code, pkgconf.stdout.as_str(), pkgconf.stderr.as_str()), (1, "", WANT));
}

/// All six comparison operators (`=`, `<`, `<=`, `>`, `>=`, `!=`) in single-arg and split-arg
/// form, including realistic version strings (`1.2.3-rc1`, `1:2.3~alpha1`).
#[test]
fn facts_pkg_config_version_constraints_all_operators() {
    let single: [&str; 8] = [
        "foo >= 1.2",
        "foo = 1.0",
        "foo < 2.0",
        "foo <= 2.0",
        "foo > 1.0",
        "foo != 1.0",
        "foo >= 1.2.3-rc1",
        "foo >= 1:2.3~alpha1",
    ];
    let want_foo = message("foo");
    for spec in single {
        let got = ask("pkg-config", &["--libs", "--cflags", spec]);
        assert_eq!((got.code, got.stdout.as_str(), &got.stderr), (1, "", &want_foo), "{spec}");
    }
    for op in ["=", "<", "<=", ">", ">=", "!="] {
        let got = ask("pkg-config", &["--libs", "--cflags", "glib-2.0", op, "2.56"]);
        assert_eq!((got.code, got.stdout.as_str(), got.stderr), (1, "", message("glib-2.0")), "split {op}");
    }
    let multi_split = ask("pkg-config", &["--libs", "--cflags", "foo", ">=", "1.0", "bar", "<", "2.0"]);
    let mut want_both = message("foo");
    want_both.push_str(message("bar").as_str());
    assert_eq!((multi_split.code, multi_split.stderr), (1, want_both));
}

/// `glib-sys` / `system-deps` form: `pkg-config --libs --cflags glib-2.0 "glib-2.0 >= 2.56"`.
/// Real pkg-config 0.29.2 does not deduplicate repeated modules: it emits one message per mention.
#[test]
fn facts_pkg_config_repeated_module_with_constraint_not_deduplicated() {
    let mut twice = message("glib-2.0");
    twice.push_str(message("glib-2.0").as_str());
    for args in [
        &["--libs", "--cflags", "glib-2.0", "glib-2.0 >= 2.56"][..],
        &["--libs", "--cflags", "glib-2.0 >= 2.56", "glib-2.0 < 3.0"][..],
        &["--libs", "--cflags", "glib-2.0", ">=", "2.56", "glib-2.0", "<", "3.0"][..],
    ] {
        let got = ask("pkg-config", args);
        assert_eq!((got.code, got.stdout.as_str(), &got.stderr), (1, "", &twice), "{args:?}");
    }
}

/// Comma-separated and multi-spec single-arg constraints.
#[test]
fn facts_pkg_config_comma_and_space_separated_constraints() {
    let mut foo_bar = message("foo");
    foo_bar.push_str(message("bar").as_str());
    for args in [
        &["--libs", "--cflags", "foo >= 1.2, bar < 2.0"][..],
        &["--libs", "--cflags", "foo >= 1.2,bar < 2.0"][..],
        &["--libs", "--cflags", "foo >= 1.2 , bar < 2.0"][..],
        &["--libs", "--cflags", "foo, bar"][..],
        &["--libs", "--cflags", "foo,bar"][..],
        &["--libs", "--cflags", "foo >= 1.2 bar < 2.0"][..],
        &["--libs", "--cflags", "foo >= 1.0,", "bar"][..],
        &["--libs", "--cflags", "foo", ",", "bar"][..],
        &["--libs", "--cflags", "foo >= 1.0", ", bar"][..],
    ] {
        let got = ask("pkg-config", args);
        assert_eq!((got.code, got.stdout.as_str(), &got.stderr), (1, "", &foo_bar), "{args:?}");
    }
    let got_four = ask("pkg-config", &["--libs", "foo > 1.0, bar <= 2.0, baz != 3.0, qux = 4.0"]);
    let mut want_four = message("foo");
    for m in ["bar", "baz", "qux"] {
        want_four.push_str(message(m).as_str());
    }
    assert_eq!((got_four.code, got_four.stderr), (1, want_four));
}

/// `--exists` (and no output flag) stay silent for version constraints unless `--print-errors`.
#[test]
fn facts_pkg_config_exists_with_version_constraint() {
    for args in [
        &["--exists", "glib-2.0 >= 2.56"][..],
        &["--exists", "glib-2.0", ">=", "2.56"][..],
        &["glib-2.0 >= 2.56"][..],
    ] {
        let silent = ask("pkg-config", args);
        assert_eq!((silent.code, silent.stdout.as_str(), silent.stderr.as_str()), (1, "", ""), "{args:?}");
    }
    let want = message("glib-2.0");
    for args in [
        &["--exists", "--print-errors", "glib-2.0 >= 2.56"][..],
        &["--exists", "glib-2.0 >= 2.56", "--print-errors"][..],
        &["--exists", "--print-errors", "glib-2.0", ">=", "2.56"][..],
        &["--print-errors", "glib-2.0 >= 2.56"][..],
    ] {
        let loud = ask("pkg-config", args);
        assert_eq!((loud.code, loud.stdout.as_str(), &loud.stderr), (1, "", &want), "{args:?}");
    }
}

/// Malformed constraints, bad operators, dangling/empty commas, empty args, and flags splitting a
/// constraint are refused by name (rule 3: never guess).
#[test]
fn facts_pkg_config_malformed_constraints_refused_by_name() {
    let cases: [(&[&str], &str); 23] = [
        (&["--libs", ">= 1.0"], ">= 1.0"),
        (&["--libs", ">=", "1.0"], ">="),
        (&["--libs", "foo >="], "foo >="),
        (&["--libs", "foo", ">="], ">="),
        (&["--libs", "foo === 1.0"], "foo === 1.0"),
        (&["--libs", "foo == 1.0"], "foo == 1.0"),
        (&["--libs", "foo ! 1.0"], "foo ! 1.0"),
        (&["--libs", "foo <> 1.0"], "foo <> 1.0"),
        (&["--libs", "foo => 1.0"], "foo => 1.0"),
        (&["--libs", "foo>=1.0"], "foo>=1.0"),
        (&["--libs", "foo >=1.0"], "foo >=1.0"),
        (&["--libs", "foo>= 1.0"], "foo>= 1.0"),
        (&["--libs", "foo >= 1.0 < 2.0"], "foo >= 1.0 < 2.0"),
        (&["--libs", "foo", ">=", "1.0", "<", "2.0"], "<"),
        (&["--libs", "foo >= >= 1.0"], "foo >= >= 1.0"),
        (&["--libs", "foo >= -1.0"], "foo >= -1.0"),
        (&["--libs", ",foo"], ",foo"),
        (&["--libs", "foo, "], "foo, "),
        (&["--libs", "foo,,bar"], "foo,,bar"),
        (&["--libs", "foo, >= 1.0"], "foo, >= 1.0"),
        (&["--libs", "foo >= , 1.0"], "foo >= , 1.0"),
        (&["--libs", ""], ""),
        (&["--libs", "   "], "   "),
    ];
    for (args, bad) in cases {
        assert_eq!(refuse(args), FactRefused::PkgConfigArgs { arg: blaze_own::String::from_str_in(bad, Alloc::global()) }, "{args:?}");
    }
    assert_eq!(
        refuse(&["--libs", "foo", ">=", "--print-errors", "1.0"]),
        FactRefused::PkgConfigArgs { arg: blaze_own::String::from_str_in("--print-errors", Alloc::global()) }
    );
    assert_eq!(
        refuse(&["--libs", "foo", "--static", ">=", "1.0"]),
        FactRefused::PkgConfigArgs { arg: blaze_own::String::from_str_in(">=", Alloc::global()) }
    );
    assert_eq!(
        refuse(&["--libs", "foo", ","]),
        FactRefused::PkgConfigArgs { arg: blaze_own::String::from_str_in(",", Alloc::global()) }
    );
    assert_eq!(
        refuse(&["--libs", "foo >= 1.0,"]),
        FactRefused::PkgConfigArgs { arg: blaze_own::String::from_str_in("foo >= 1.0,", Alloc::global()) }
    );
}

/// Newlines in any argument (including with `--version`) are refused by name so `facts::subject`'s
/// newline-delimited encoding stays injective.
#[test]
fn facts_pkg_config_newlines_refused_by_name() {
    let cases: [(&[&str], &str); 5] = [
        (&["--libs", "foo\nbar"], "foo\nbar"),
        (&["--libs", "foo >= 1.0\n"], "foo >= 1.0\n"),
        (&["--version", "foo\nbar"], "foo\nbar"),
        (&["--libs\n", "foo"], "--libs\n"),
        (&["--libs", "foo\rbar"], "foo\rbar"),
    ];
    for (args, bad) in cases {
        assert_eq!(refuse(args), FactRefused::PkgConfigArgs { arg: blaze_own::String::from_str_in(bad, Alloc::global()) }, "{args:?}");
    }
}

/// `sw_vers -productVersion` / `-buildVersion` print the pins, one line; two options are not a
/// fact (sw_vers takes one). On a host declaring no Apple userland (empty macOS pins), `sw_vers`
/// is refused by name while compiler identity and `pkg-config` still answer.
#[test]
fn facts_sw_vers_answers_the_pins() {
    assert_eq!(ask("/usr/bin/sw_vers", &["-productVersion"]).stdout, "26.1\n");
    assert_eq!(ask("sw_vers", &["-buildVersion"]).stdout, "25B78\n");
    assert_eq!(ask("sw_vers", &["-productName"]).stdout, "macOS\n");
    assert_eq!(question("sw_vers", &["-productVersion", "-buildVersion"]), None);

    let linux_pins = Pins {
        clang_version_text: "clang version 23.1.4\nTarget: aarch64-unknown-linux-gnu\nThread model: posix\nInstalledDir: /usr/bin\n",
        macos_product_version: "",
        macos_build_version: "",
    };
    for opt in ["-productVersion", "-buildVersion", "-productName"] {
        let args = [opt];
        let fact = question("sw_vers", &args).expect("a derived fact");
        assert_eq!(answer(&fact, &linux_pins, Alloc::global()), Err(FactRefused::NoAppleUserland));
    }
    let cc = question("cc", &["--version"]).expect("a derived fact");
    assert_eq!(answer(&cc, &linux_pins, Alloc::global()).expect("answered").stdout, linux_pins.clang_version_text);
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
    assert_eq!(subject(&Fact::PkgConfig { args: &[] }, g), "pkg-config");
    assert_eq!(
        subject(&Fact::PkgConfig { args: &["--libs", "glib-2.0 >= 2.56"] }, g),
        "pkg-config\n--libs\nglib-2.0 >= 2.56"
    );
    assert_eq!(subject(&Fact::SwVers { args: &["-productVersion"] }, g), "sw_vers\n-productVersion");
    assert_eq!(subject(&Fact::CompilerVersion { program: "/usr/bin/clang", arg: "--version" }, g), "cc-version\nclang\n--version");
}
