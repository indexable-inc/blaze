//! Elision of real build scripts (a census of crates.io's and this tree's, each named by where
//! it is from, its source as published), of the forms it reads, and of each form it refuses.

use blaze_ids::rng::SplitMix64;

use super::*;
use crate::{
    output::{self, Context, Output},
    unit::Package,
};

fn elided(source: &str) -> Option<String> {
    elided_bytes(source.as_bytes())
}

fn elided_bytes(source: &[u8]) -> Option<String> {
    let files = BTreeMap::from([(PathBuf::from("/p/foo/build.rs"), source.to_vec())]);
    elide(Path::new("/p/foo/build.rs"), &files).map(|it| String::from_utf8(it).unwrap())
}

/// Build scripts as published, and what elision says of each: what they print, or `None`.
const CENSUS: &[(&str, &str, Option<&str>)] = &[
    (
        "wasm-bindgen 0.2.100",
        "// Empty `build.rs` so that `[package] links = ...` works in `Cargo.toml`.
fn main() {
    println!(\"cargo:rerun-if-changed=build.rs\");
}
",
        Some("cargo:rerun-if-changed=build.rs\n"),
    ),
    (
        "rayon-core 1.13.0",
        "// We need a build script to use `link = \"rayon-core\"`.  But we're not
// *actually* linking to anything, just making sure that we're the only
// rayon-core in use.
fn main() {
    // we don't need to rebuild for anything else
    println!(\"cargo:rerun-if-changed=build.rs\");
}
",
        Some("cargo:rerun-if-changed=build.rs\n"),
    ),
    (
        "mockall_derive 0.15.0",
        "fn main() {
    // Avoid unnecessary re-building.
    println!(\"cargo:rerun-if-changed=build.rs\");

    // Quiet rustc's helpful lint.  reprocheck is defined in CI.
    println!(\"cargo:rustc-check-cfg=cfg(reprocheck)\");
}
",
        Some("cargo:rerun-if-changed=build.rs\ncargo:rustc-check-cfg=cfg(reprocheck)\n"),
    ),
    (
        "mockall 0.15.0",
        r#"fn main() {
    println!("cargo::rustc-check-cfg=cfg(target_os, values(\"multics\"))");
}
"#,
        Some("cargo::rustc-check-cfg=cfg(target_os, values(\"multics\"))\n"),
    ),
    (
        "dashu-int 0.6.2",
        r###"//! Declare the `force_bits` cfg that the test workflow sets via
//! `RUSTFLAGS=--cfg force_bits=...` (16/32/64) so the matrix can exercise
//! the alternative `Word` widths. Without this declaration, stable Rust's
//! `unexpected_cfgs` lint (added by default in 1.80) fires on the three
//! `#[cfg(force_bits = "...")]` sites in `src/arch/mod.rs`.
fn main() {
    // Single-colon `cargo:` form for portability. The directive itself
    // was stabilised in Rust 1.80; older cargo (down to our MSRV) just
    // silently ignores unknown `cargo:` directives, which is fine because
    // the `unexpected_cfgs` lint they'd suppress doesn't exist at older
    // versions either.
    println!(r#"cargo:rustc-check-cfg=cfg(force_bits, values("16", "32", "64"))"#);
}
"###,
        Some("cargo:rustc-check-cfg=cfg(force_bits, values(\"16\", \"32\", \"64\"))\n"),
    ),
    (
        "screencapturekit-sys 0.2.8",
        "fn main() {
    println!(\"cargo:rustc-link-lib=framework=CoreFoundation\");
    println!(\"cargo:rustc-link-lib=framework=CoreMedia\");
    println!(\"cargo:rustc-link-lib=framework=CoreVideo\");
    println!(\"cargo:rustc-link-lib=framework=Foundation\");
    println!(\"cargo:rustc-link-lib=framework=CoreGraphics\");
    println!(\"cargo:rustc-link-lib=framework=CoreImage\");
    println!(\"cargo:rustc-link-lib=framework=ImageIO\");
    println!(\"cargo:rustc-link-lib=framework=ScreenCaptureKit\");
}
",
        Some(
            "cargo:rustc-link-lib=framework=CoreFoundation\n\
             cargo:rustc-link-lib=framework=CoreMedia\n\
             cargo:rustc-link-lib=framework=CoreVideo\n\
             cargo:rustc-link-lib=framework=Foundation\n\
             cargo:rustc-link-lib=framework=CoreGraphics\n\
             cargo:rustc-link-lib=framework=CoreImage\n\
             cargo:rustc-link-lib=framework=ImageIO\n\
             cargo:rustc-link-lib=framework=ScreenCaptureKit\n",
        ),
    ),
    (
        "tango-bench 0.6.0",
        "fn main() {
    println!(\"cargo:rustc-link-arg-benches=-rdynamic\");
    println!(\"cargo:rerun-if-changed=build.rs\");
}
",
        Some("cargo:rustc-link-arg-benches=-rdynamic\ncargo:rerun-if-changed=build.rs\n"),
    ),
    (
        "decancer 3.3.3",
        "fn main() {
  println!(\"cargo:rerun-if-changed=bin/bidi.bin\");
  println!(\"cargo:rerun-if-changed=bin/codepoints.bin\");
}
",
        Some("cargo:rerun-if-changed=bin/bidi.bin\ncargo:rerun-if-changed=bin/codepoints.bin\n"),
    ),
    (
        "defmt-macros 1.2.0",
        "fn main() {
    println!(\"cargo:rerun-if-env-changed=DEFMT_LOG\");
}
",
        Some("cargo:rerun-if-env-changed=DEFMT_LOG\n"),
    ),
    (
        "biscuit-auth 6.0.0",
        "/*
 * Copyright (c) 2019 Geoffroy Couprie <contact@geoffroycouprie.com> and Contributors to the Eclipse Foundation.
 * SPDX-License-Identifier: Apache-2.0
 */
fn main() {
    println!(\"cargo:rerun-if-changed=src/format/schema.proto\");
    //prost_build::compile_protos(&[\"src/format/schema.proto\"], &[\"src/\"]).unwrap();
}
",
        Some("cargo:rerun-if-changed=src/format/schema.proto\n"),
    ),
    (
        "wit-bindgen-rust 0.62.0",
        "fn main() {
    println!(\"cargo:rerun-if-changed=build.rs\");
    // this build script is currently only here so OUT_DIR is set for testing.
}
",
        Some("cargo:rerun-if-changed=build.rs\n"),
    ),
    (
        "wayland-client 0.31.15",
        "fn main() {
    println!(\"cargo:rustc-check-cfg=cfg(coverage)\");
    println!(\"cargo:rustc-check-cfg=cfg(unstable_coverage)\");
}
",
        Some("cargo:rustc-check-cfg=cfg(coverage)\ncargo:rustc-check-cfg=cfg(unstable_coverage)\n"),
    ),
    (
        "num-bigint-dig 0.9.1",
        "fn main() {\n    println!(\"cargo:rustc-cfg=has_i128\");\n}\n",
        Some("cargo:rustc-cfg=has_i128\n"),
    ),
    ("embassy-time-driver 0.1.0", "fn main() {}\n", Some("")),
    ("blaze-manifest's fixtures/inherit/a", "fn main(){}", Some("")),
    (
        "proc-macro-srv's proc-macro-test/imp",
        "//! This teaches cargo about our cfg(rust_analyzer)

fn main() {
    println!(\"cargo:rustc-check-cfg=cfg(rust_analyzer)\");
}
",
        Some("cargo:rustc-check-cfg=cfg(rust_analyzer)\n"),
    ),
    (
        "harness/buildmodel/links/dep",
        "fn main() {\n    println!(\"cargo:answer=42\");\n    println!(\"cargo:rerun-if-changed=build.rs\");\n}\n",
        Some("cargo:answer=42\ncargo:rerun-if-changed=build.rs\n"),
    ),
    (
        "harness/buildmodel/reload_git_checkout/next/app",
        "fn main() {\n    println!(\"cargo:rustc-cfg=scripted\");\n    println!(\"cargo:rustc-env=BM_FROM_SCRIPT=first\");\n}\n",
        Some("cargo:rustc-cfg=scripted\ncargo:rustc-env=BM_FROM_SCRIPT=first\n"),
    ),
    // Refused, each for what it does besides printing constants.
    ("blaze-manifest's fixtures/discovery/crates/app: empty, no main", "", None),
    (
        "prettyplease 0.1.25: concat! and env!",
        "fn main() {
    println!(\"cargo:rerun-if-changed=build.rs\");

    println!(concat!(\"cargo:VERSION=\", env!(\"CARGO_PKG_VERSION\")));
}
",
        None,
    ),
    (
        "bollard-buildkit-proto 0.7.0: main returns a Result",
        "fn main() -> Result<(), Box<dyn std::error::Error>> {\n    Ok(())\n}\n",
        None,
    ),
    (
        "cpal 0.18.2: a use, a const, a captured argument and a branch",
        "use std::env;

const CPAL_ASIO_DIR: &str = \"CPAL_ASIO_DIR\";

fn main() {
    println!(\"cargo:rerun-if-env-changed={CPAL_ASIO_DIR}\");
    if env::var(CPAL_ASIO_DIR).is_ok() {
        println!(\"cargo:rustc-cfg=asio\");
    }
}
",
        None,
    ),
    (
        "libafl_bolts 0.16.1: attributes and another function",
        "#[rustversion::nightly]
fn nightly() {
    println!(\"cargo:rustc-cfg=nightly\");
}

#[rustversion::not(nightly)]
fn nightly() {}

fn main() {
    println!(\"cargo:rerun-if-changed=build.rs\");
    println!(\"cargo:rustc-check-cfg=cfg(nightly)\");
    nightly();
    #[cfg(target_env = \"musl\")]
    println!(\"cargo:rustc-link-lib=ucontext\");
}
",
        None,
    ),
    (
        "moka 0.12.10: a main behind a cfg",
        "#[cfg(rustver)]
fn main() {
    use rustc_version::version;
    let version = version().expect(\"Can't get the rustc version\");
    println!(
        \"cargo:rustc-env=RUSTC_SEMVER={}.{}\",
        version.major, version.minor
    );
}

#[cfg(not(rustver))]
fn main() {}
",
        None,
    ),
    (
        "inkwell 0.11.0: cfg!",
        "fn main() {
    if cfg!(all(not(target_os = \"windows\"), not(feature = \"no-libffi-linking\"))) {
        println!(\"cargo:rustc-link-lib=dylib=ffi\");
    }
}
",
        None,
    ),
    (
        "borsh 1.5.7: another macro",
        "use cfg_aliases::cfg_aliases;

fn main() {
    cfg_aliases! {
        hash_collections: { any(feature = \"hashbrown\", feature = \"std\") },
    }
}
",
        None,
    ),
    ("chrono-tz 0.10.0: a dependency's function", "fn main() {\n    chrono_tz_build::main();\n}\n", None),
    (
        "harness/buildmodel/opt_level_dev: reads the environment",
        "fn main() {
    for (name, var) in [(\"BUILD_PROFILE\", \"PROFILE\"), (\"BUILD_OPT\", \"OPT_LEVEL\"), (\"BUILD_DEBUG\", \"DEBUG\")] {
        println!(\"cargo:rustc-env={name}={}\", std::env::var(var).unwrap());
    }
}
",
        None,
    ),
    (
        "libm (compiler-builtins): a module",
        "mod configure;

use configure::{Config, Library};

fn main() {
    println!(\"cargo:rerun-if-changed=build.rs\");
    println!(\"cargo:rerun-if-changed=configure.rs\");

    let cfg = Config::from_env(Library::Libm);
    configure::emit(&cfg);
}
",
        None,
    ),
    (
        "oid-registry 0.8.1: include!",
        "use std::env;

include!(\"src/load.rs\");

fn main() -> Result<()> {
    println!(\"cargo:rerun-if-changed=assets/oid_db.txt\");

    let out_dir = env::var_os(\"OUT_DIR\").unwrap();
    let dest_path = Path::new(&out_dir).join(\"oid_db.rs\");

    let m = load_file(\"assets/oid_db.txt\")?;
    generate_file(&m, dest_path)?;

    Ok(())
}
",
        None,
    ),
    (
        "encoding_rs 0.8.42: a macro of a dependency",
        "fn main() {
    println!(\"cargo::rustc-check-cfg=cfg(slow_mm_packus_epi16)\");
    if rustversion::cfg!(all(since(1.96), before(1.99))) {
        println!(\"cargo:rustc-cfg=slow_mm_packus_epi16\");
    }
}
",
        None,
    ),
    (
        "proc-macro-srv: runs rustc",
        "//! Determine rustc version `proc-macro-srv` (and thus the sysroot ABI) is
//! build with and make it accessible at runtime for ABI selection.

use std::{env, process::Command};

fn main() {
    let tool = env::var(\"RUSTC\").expect(\"proc-macro-srv's build script expects RUSTC to be set\");
    #[allow(clippy::disallowed_methods)]
    let output = Command::new(tool).arg(\"--version\").output().expect(\"rustc --version must run\");
    let version_string = std::str::from_utf8(&output.stdout[..])
        .expect(\"rustc --version output must be UTF-8\")
        .trim();
    println!(\"cargo::rustc-env=RUSTC_VERSION={version_string}\");
}
",
        None,
    ),
];

#[test]
fn census() {
    for (from, source, expected) in CENSUS {
        assert_eq!(elided(source).as_deref(), *expected, "{from}");
    }
}

/// Each form elision refuses, alone in an otherwise inert script.
#[test]
fn every_rejection() {
    for source in [
        // Attributes, items and other ways to change `main`, `println!` or what is read.
        "#![allow(unused)] fn main() {}",
        "#[inline] fn main() {}",
        "fn main() { #[cfg(unix)] println!(\"cargo:rustc-cfg=u\"); }",
        "extern crate std; fn main() {}",
        "extern \"C\" fn main() {}",
        "use std::io; fn main() {}",
        "mod a; fn main() {}",
        "macro_rules! println { ($($t:tt)*) => {} } fn main() { println!(\"cargo:x=1\"); }",
        "fn main() { println!(\"cargo:x=1\"); } fn other() {}",
        "fn helper() {} fn main() {}",
        "fn main() {} ;",
        "#!/usr/bin/env run-cargo-script\nfn main() {}",
        // Another `main`.
        "fn main() -> () {}",
        "pub fn main() {}",
        "async fn main() {}",
        "unsafe fn main() {}",
        "const fn main() {}",
        "fn r#main() {}",
        "fn main(_: u8) {}",
        "fn main<T>() {}",
        "fn main() where {}",
        // Other statements, macros and paths.
        "fn main() { let x = 1; }",
        "fn main() { ; }",
        "fn main() { {} }",
        "fn main() { std::println!(\"cargo:x=1\"); }",
        "fn main() { ::std::println!(\"cargo:x=1\"); }",
        "fn main() { r#println!(\"cargo:x=1\"); }",
        "fn main() { eprintln!(\"cargo:x=1\"); }",
        "fn main() { format!(\"cargo:x=1\"); }",
        "fn main() { println(\"cargo:x=1\"); }",
        "fn main() { if cfg!(unix) { println!(\"cargo:rustc-cfg=u\"); } }",
        "fn main() { println!(include_str!(\"x\")); }",
        "fn main() { println!(\"cargo:rustc-env=A={}\", env!(\"X\")); }",
        "fn main() { println!(\"cargo:rustc-env=A={}\", x); }",
        "fn main() { println!(\"cargo:rustc-env=A={}\", main); }",
        "fn main() { println!(\"cargo:rustc-env=A={}\", (\"a\")); }",
        "fn main() { println!(\"cargo:rustc-env=A={}\", [\"a\"]); }",
        "fn main() { println!(\"cargo:rustc-env=A={}\", \"a\".len()); }",
        // Statements that need their `;`, and an empty statement after a braced macro.
        "fn main() { println!(\"cargo:x=1\") println!(\"cargo:y=1\"); }",
        "fn main() { println![\"cargo:x=1\"] println!(\"cargo:y=1\"); }",
        "fn main() { println!{\"cargo:x=1\"}; }",
        "fn main() { println!(\"cargo:x=1\"); ; }",
        "fn main() { println!(\"cargo:x=1\"]; }",
        // Format strings and arguments.
        "fn main() { println!(); }",
        "fn main() { print!(); }",
        "fn main() { println!(\"cargo:x={y}\"); }",
        "fn main() { println!(\"cargo:x={:?}\", \"a\"); }",
        "fn main() { println!(\"cargo:x={:}\", \"a\"); }",
        "fn main() { println!(\"cargo:x={0:}\", \"a\"); }",
        "fn main() { println!(\"cargo:x={:>4}\", \"a\"); }",
        "fn main() { println!(\"cargo:x={ }\", \"a\"); }",
        "fn main() { println!(\"cargo:x={00}\", \"a\"); }",
        "fn main() { println!(\"cargo:x={}\"); }",
        "fn main() { println!(\"cargo:x=\", \"a\"); }",
        "fn main() { println!(\"cargo:x={1}\", \"a\"); }",
        "fn main() { println!(\"cargo:x={0}\", \"a\", \"b\"); }",
        "fn main() { println!(\"cargo:x={}\", x = \"a\"); }",
        "fn main() { println!(\"cargo:x=}\"); }",
        "fn main() { println!(\"cargo:x={\"); }",
        "fn main() { println!(\"cargo:x={}\", \"a\",,); }",
        "fn main() { println!(\"cargo:x=1\" \"a\"); }",
        "fn main() { println!(\"cargo:x=1\";); }",
        r#"fn main() { println!("cargo:x=\u{7b}\u{7d}", 1); }"#,
        r#"fn main() { println!("cargo:x=\x7b}}"); }"#,
        "fn main() { println!(b\"cargo:x=1\"); }",
        "fn main() { println!(c\"cargo:x=1\"); }",
        "fn main() { println!('c'); }",
        "fn main() { println!(1); }",
        "fn main() { println!(\"cargo:x={}\", 3000000000); }",
        "fn main() { println!(\"cargo:x={}\", -1); }",
        "fn main() { println!(\"cargo:x={}\", 1u8); }",
        "fn main() { println!(\"cargo:x={}\", 0x10); }",
        "fn main() { println!(\"cargo:x={}\", 1.5); }",
        "fn main() { println!(\"cargo:x={}\", b'a'); }",
        "fn main() { println!(\"cargo:x={}\", 'a); }",
        "fn main() { println!(\"cargo:x={}\", r#true); }",
        "fn main() { println!(\"cargo:x={}\", None); }",
        "fn main() { println!(\"cargo:x=1\"suffix); }",
        // What is printed: a line that is not a directive, or output not ended.
        "fn main() { println!(\"hello\"); }",
        "fn main() { println!(\"cargo:x=1\\nhello\"); }",
        "fn main() { println!(\" cargo:x=1\"); }",
        "fn main() { print!(\"cargo:x=1\"); }",
        "fn main() { println!(\"cargo:x=1\\n\"); }",
        // Doc comments where rustc refuses them or reads them as attributes of a statement.
        "fn main() { /// doc\n println!(\"cargo:x=1\"); }",
        "fn main() { println!(\"cargo:x=1\"); /** doc */ }",
        "/// a\n//! b\nfn main() {}",
        "fn main() {}\n/// trailing",
        "fn main() {}\n//! inner, late",
        // Sources that do not lex or do not close.
        "fn main() { println!(\"cargo:x=1\"); /* unterminated }",
        "fn main() { println!(\"cargo:x=1); }",
        "fn main() { println!(\"cargo:x=1\");",
        "fn main() { println!(\"cargo:x=1\"); }}",
        "fn main() {\r println!(\"cargo:x=1\"); }",
        "fn main() { println!(\"cargo:x=\u{202e}1\"); }",
        "fn main() { println!(\"cargo:x=1\"); } // \u{2067}",
        "fn mäin() {}",
    ] {
        assert_eq!(elided(source), None, "{source:?}");
    }
    assert_eq!(elided_bytes(b"fn main() { println!(\"cargo:x=\xff\"); }"), None, "not UTF-8");
    let files = BTreeMap::from([(PathBuf::from("/p/foo/other.rs"), b"fn main() {}".to_vec())]);
    assert_eq!(elide(Path::new("/p/foo/build.rs"), &files), None, "no root");
}

/// The forms read, each printing what rustc's program would.
#[test]
fn what_each_form_prints() {
    for (source, printed) in [
        (
            r#"fn main() { println!("cargo:rustc-env=A={}-{}-{}-{}-{}", "s", 'c', 42, true, false); }"#,
            "cargo:rustc-env=A=s-c-42-true-false\n",
        ),
        (
            r#"fn main() { println!("cargo:rustc-env=A={1}{0}{1}{}", "x", "y"); }"#,
            "cargo:rustc-env=A=yxyx\n",
        ),
        (r#"fn main() { println!("cargo:x={}", 2147483647); }"#, "cargo:x=2147483647\n"),
        (r#"fn main() { println!("cargo:x={}", 1_000); }"#, "cargo:x=1000\n"),
        (r#"fn main() { println!("cargo:x={}", 007); }"#, "cargo:x=7\n"),
        (r#"fn main() { println!("cargo:x={}", "{}"); }"#, "cargo:x={}\n"),
        (r#"fn main() { println!("cargo:x={}", "\u{7b}"); }"#, "cargo:x={\n"),
        (r#"fn main() { println!("cargo:x={{}}"); }"#, "cargo:x={}\n"),
        (r#"fn main() { println!("cargo:x={{{}}}", "a"); }"#, "cargo:x={a}\n"),
        (r#"fn main() { println!("cargo:x={}", "a",); }"#, "cargo:x=a\n"),
        (r#"fn main() { println!("cargo:x=1",); }"#, "cargo:x=1\n"),
        (
            r#"fn main() { println!("cargo:rustc-env=A=\x41\u{42}\t\\\"\'\0"); }"#,
            "cargo:rustc-env=A=AB\t\\\"'\0\n",
        ),
        (r###"fn main() { println!(r##"cargo:x="#"##); }"###, "cargo:x=\"#\n"),
        (r###"fn main() { println!(r#"cargo:x={}"#, r"a\"); }"###, "cargo:x=a\\\n"),
        (r#"fn main() { println!(r"cargo:x={}\n", "a"); }"#, "cargo:x=a\\n\n"),
        (
            "fn main() { print!(\"cargo:a=1\\n\"); print!(\"cargo:b=2\\ncargo:c=\"); print!(\"3\\n\") }",
            "cargo:a=1\ncargo:b=2\ncargo:c=3\n",
        ),
        (
            "fn main() { println!{\"cargo:a=1\"} println![\"cargo:b=2\"]; println![\"cargo:c=3\"] }",
            "cargo:a=1\ncargo:b=2\ncargo:c=3\n",
        ),
        ("fn main() { println!(\"cargo:a=1\") }", "cargo:a=1\n"),
        (
            "fn main ( ) { println /* a comment */ ! ( \"cargo:a=1\" ) ; }",
            "cargo:a=1\n",
        ),
        ("fn main() { println!(\"cargo:a=\\\n      1\"); }", "cargo:a=1\n"),
        ("fn main() { println!(\"cargo:a=1\ncargo:b=2\"); }", "cargo:a=1\ncargo:b=2\n"),
        ("fn main() { println!(\"cargo:a=é\u{301}✓\"); }", "cargo:a=é\u{301}✓\n"),
        // rustc reads a file with `\r\n` as with `\n`, string literals too.
        (
            "fn main() {\r\n    println!(\"cargo:a=1\r\ncargo:b=2\");\r\n}\r\n",
            "cargo:a=1\ncargo:b=2\n",
        ),
        ("\u{feff}fn main() { println!(\"cargo:a=1\"); }", "cargo:a=1\n"),
        ("/* a /* nested */ comment */ fn main() {}", ""),
        ("/** outer */ /// outer too\nfn main() {}", ""),
        ("//! inner\n/*! inner too */\n/// outer\nfn main() {}", ""),
        ("/**/ /***/ //// plain\nfn main() {}", ""),
        ("fn main() { println!(\"cargo:a=1\"); } // done\n", "cargo:a=1\n"),
    ] {
        assert_eq!(elided(source).as_deref(), Some(printed), "{source:?}");
    }
}

/// What is elided goes through cargo's reading of it as a run's output does.
#[test]
fn read_as_a_runs_output() {
    let package = Package { name: "foo".into(), version: "0.1.0".into(), ..Package::default() };
    let out = Path::new("/p/foo/target/debug/build/foo-1/out");
    let cx = Context {
        package: &package,
        out_dir_when_generated: out,
        out_dir: out,
        nightly_features_allowed: false,
        rustc_bootstrap: None,
    };
    let printed = elided(
        "fn main() {
    println!(\"cargo:rerun-if-changed=build.rs\");
    println!(\"cargo::rustc-check-cfg=cfg(foo)\");
    println!(\"cargo:rustc-cfg=foo\");
    println!(\"cargo:links=1\");
}",
    )
    .unwrap();
    let output = output::parse(printed.as_bytes(), &cx).unwrap();
    assert_eq!(output, Output {
        cfgs: vec!["foo".into()],
        check_cfgs: vec!["cfg(foo)".into()],
        metadata: vec![("links".into(), "1".into())],
        rerun_if_changed: vec!["build.rs".into()],
        ..Output::default()
    });
}

#[test]
fn verify_names_the_first_line_that_differs() {
    assert_eq!(verify(b"cargo:a=1\ncargo:b=2\n", b"cargo:a=1\ncargo:b=2\n"), Ok(()));
    assert_eq!(verify(b"", b""), Ok(()));
    assert_eq!(
        verify(b"cargo:a=1\ncargo:b=2\n", b"cargo:a=1\ncargo:b=3\n").unwrap_err().0,
        "the elided build script's output differs from its run's at line 2: elision says \
         \"cargo:b=2\\n\", the run printed \"cargo:b=3\\n\""
    );
    assert_eq!(
        verify(b"cargo:a=1\n", b"cargo:a=1\ncargo:b=2\n").unwrap_err().0,
        "the elided build script's output differs from its run's at line 2: elision says \
         nothing, the run printed \"cargo:b=2\\n\""
    );
    assert_eq!(
        verify(b"cargo:a=1\n", b"cargo:a=1").unwrap_err().0,
        "the elided build script's output differs from its run's at line 1: elision says \
         \"cargo:a=1\\n\", the run printed \"cargo:a=1\""
    );
}

#[test]
fn flags_that_leave_elision_true() {
    let flags = |it: &[&str]| it.iter().map(|it| (*it).to_owned()).collect::<Vec<_>>();
    for allowed in [
        flags(&[]),
        flags(&["-C", "opt-level=3", "-Cdebuginfo=2", "-Copt-level=s"]),
        flags(&["--cfg", "foo", "--cfg", "feature=\"std\"", "--cfg=bar"]),
        flags(&["-C", "debuginfo=line-tables-only"]),
    ] {
        assert!(flags_allow(&allowed), "{allowed:?}");
    }
    for refused in [
        flags(&["-D", "warnings"]),
        flags(&["-Dwarnings"]),
        flags(&["-Dmissing-docs"]),
        flags(&["-Z", "crate-attr=no_std"]),
        flags(&["-C", "link-arg=-fuse-ld=mold"]),
        flags(&["-C", "target-cpu=native"]),
        flags(&["-C", "opt-level=4"]),
        flags(&["-C"]),
        flags(&["--cfg"]),
        flags(&["--cfg", "a b"]),
        flags(&["--cfg", "x=\"a\\\"b\""]),
        flags(&["--cfg", "x=unquoted"]),
        flags(&["--check-cfg", "cfg(foo)"]),
        flags(&["--extern", "std=/x"]),
        flags(&["--edition", "2015"]),
    ] {
        assert!(!flags_allow(&refused), "{refused:?}");
    }
}

/// Elision reading tokens on demand against elision over the whole token list
/// ([`reference::elide`]), over seeded scripts of every form it reads and the forms around
/// them: a head with docs, comments, attributes or another signature; statements of each
/// macro form, delimiter and argument kind, escapes in the format string, unused and missing
/// arguments; a tail with nothing, a comment, or another item. The same bytes, or `None`.
#[test]
fn on_demand_elision_says_what_the_whole_list_said() {
    let heads = [
        "",
        "//! Inner.\n",
        "//! Inner.\n/// Outer.\n",
        "/// Outer.\n//! Inner after outer.\n",
        "// Plain.\n/* block */\n",
        "use std::env;\n",
        "#![allow(x)]\n",
        "\u{feff}",
        " \t\n\x0b\x0c",
        "\u{200e}",
        "\u{a0}",
        "\u{feff}  /* lead */ ",
        "extern crate x;\n",
        "#!/usr/bin/env run\n",
    ];
    let mains =
        ["fn main() {", "fn main(){\n", "pub fn main() {", "fn main() -> () {", "fn main ( ) {"];
    let statements = [
        "\n    println!(\"cargo:rustc-cfg=a\");",
        "\n    print!(\"cargo:x={}\\n\", 1);",
        "\n    println![\"cargo:{}={}\", \"k\", 'v'];",
        "\n    println!{\"cargo:{0}{0}\", true}",
        "\n    println!{\"cargo:x\"};",
        "\n    println!(\"cargo:{{x}}\");",
        "\n    println!(\"cargo:{}\", 2147483647);",
        "\n    println!(\"cargo:{}\", 2147483648);",
        "\n    println!(\"cargo:\\u{7b}\");",
        "\n    println!(\"cargo:\\x41\\n{}\", \"\\u{e9}\\\"\");",
        "\n    println!(\"not cargo\");",
        "\n    println!(r#\"cargo:raw={}\"#, r\"r\");",
        "\n    println!(\"cargo:{}\", x);",
        "\n    println!(\"cargo:{1}\", \"a\", \"b\");",
        "\n    println!(\"cargo:{1}{0}\", \"a\", \"b\",);",
        "\n    println!(\"cargo:a\",);",
        "\n    println!(\"cargo:a\\\n        b\");",
        "\n    println!(\"cargo:{:?}\", 1);",
        "\n    println!(\"cargo:{}\", \"{\");",
        "\n    print!(\"cargo:x\");",
        "\n    println!(\"cargo:x\")",
        "\n    ;",
        "\n    std::env::var(\"X\");",
        "\n    println!(\"cargo:{}\", 0x1);",
        "\n    println!(\"cargo:{}\", '\\n');",
        "\n    println!(\"cargo:{} {}\", 1);",
        "\n    println!(\"cargo:{01}\", 1, 2);",
        "\n    // A comment.\n    println!(\"cargo:c\");",
    ];
    let tails = ["\n}\n", "}", "\n}\nfn other() {}\n", "\n} // End.\n", "\n}}\n", "\n"];
    let mut rng = SplitMix64::new(0x3c6e_f372_fe94_f82b);
    let root = Path::new("/p/foo/build.rs");
    let (mut read, mut refused) = (0, 0);
    for _ in 0..60_000 {
        let mut source = String::from(heads[rng.index(heads.len())]);
        source.push_str(mains[rng.index(mains.len())]);
        for _ in 0..rng.index(5) {
            source.push_str(statements[rng.index(statements.len())]);
        }
        source.push_str(tails[rng.index(tails.len())]);
        if rng.one_in(8) {
            source = source.replace('\n', "\r\n");
        }
        let files = BTreeMap::from([(root.to_owned(), source.clone().into_bytes())]);
        let new = elide(root, &files);
        assert_eq!(new, reference::elide(root, &files), "{source:?}");
        match new {
            Some(_) => read += 1,
            None => refused += 1,
        }
    }
    assert!(read > 1_000 && refused > 1_000, "{read} read, {refused} refused");
}

/// A placeholder may name its argument any number of times, so what a script prints can grow
/// as the square of its source: past [`PRINTED_PER_BYTE`] bytes per byte of the root file it is
/// run, not read. The input is safety-fuzz-usercode's
/// (`fuzz/corpus/usercode_script_elide/a-placeholder-reused-past-the-bound.hex`): 9046 bytes
/// that would print 6 MB.
#[test]
fn a_placeholder_reused_past_the_bound_is_run() {
    let script = |times: usize| {
        let (spec, arg) = ("{0}".repeat(times), "a".repeat(3000));
        format!("fn main() {{ println!(\"cargo:warning={spec}\", \"{arg}\"); }}\n")
    };
    let source = script(2000);
    assert_eq!(source.len(), 9046);
    assert_eq!(elided(&source), None);
    assert_eq!(elided(&script(2)).unwrap(), format!("cargo:warning={}\n", "a".repeat(6000)));
    // 15 copies print 45,015 bytes from 3,091 (the bound is 49,456); 17 print 51,015 from
    // 3,097 (the bound is 49,552).
    assert_eq!(elided(&script(15)).map(|it| it.len()), Some(45_015));
    assert_eq!(elided(&script(17)), None);
}

/// The first bytes refuse what cannot open `fn main`, and leave the rest to the lexer: a byte
/// order mark and ASCII white space are passed over, a byte not ASCII may be white space.
#[test]
fn refused_at_the_first_byte() {
    for opens in ["", "fn main() {}", "  //! d", "\u{feff}\n/* c */", "\u{200e}fn", "f"] {
        assert!(may_open_main(opens.as_bytes()), "{opens:?}");
    }
    for refused in ["use std;", " \t#![x]", "\u{feff}pub fn main() {}", "r#fn", "\r\nmod a;"] {
        assert!(!may_open_main(refused.as_bytes()), "{refused:?}");
    }
}

/// A refused script allocates nothing; a read one, nothing once the caller's buffer, the
/// thread's frame and its lent strings have grown: what it prints goes into the caller's
/// buffer, everything else is bumped in the frame and freed with the task.
#[test]
fn what_elision_allocates() {
    let root = Path::new("/p/foo/build.rs");
    let refused = BTreeMap::from([(
        root.to_owned(),
        b"use std::env;\nfn main() { println!(\"cargo:rustc-cfg=x\"); }\n".to_vec(),
    )]);
    let mut out = Vec::new();
    let kept = blaze_alloc_count::assert_at_most(0, "a script refused at its first byte", || {
        elide_into(root, &refused, &mut out)
    });
    assert!(!kept && out.is_empty());
    let read = BTreeMap::from([(
        root.to_owned(),
        b"// Generated.\r\nfn main() {\r\n    println!(\"cargo:rustc-cfg=has_foo\");\r\n    \
          println!(\"cargo:rustc-check-cfg=cfg(has_foo)\");\r\n    \
          println!(\"cargo:rustc-env=LEVEL={}\\t\", 3);\r\n}\r\n"
            .to_vec(),
    )]);
    let printed =
        "cargo:rustc-cfg=has_foo\ncargo:rustc-check-cfg=cfg(has_foo)\ncargo:rustc-env=LEVEL=3\t\n";
    let cold = "a script read cold: the caller's buffer and the lent string";
    let kept = blaze_alloc_count::assert_at_most(2, cold, || elide_into(root, &read, &mut out));
    assert!(kept);
    assert_eq!(std::str::from_utf8(&out).unwrap(), printed);
    let warm = "a script read warm";
    let kept = blaze_alloc_count::assert_at_most(0, warm, || elide_into(root, &read, &mut out));
    assert!(kept);
    assert_eq!(std::str::from_utf8(&out).unwrap(), printed);
    assert!(!elide_into(root, &refused, &mut out) && out.is_empty(), "a refusal empties it");
    assert_eq!(elide(root, &read).as_deref(), Some(printed.as_bytes()), "elide is elide_into's");
}

mod reference;
