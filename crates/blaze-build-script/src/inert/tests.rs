use super::*;

fn inert(source: &str) -> Inert {
    of(source, Edition::Edition2021).unwrap_or_else(|why| panic!("{why}: {source}"))
}

fn printed(source: &str, env: &[(&str, &str)]) -> (String, String) {
    let printed = inert(source).run(|name| env.iter().rev().find(|it| it.0 == name).map(|it| it.1));
    (String::from_utf8(printed.stdout).unwrap(), String::from_utf8(printed.stderr).unwrap())
}

fn stdout(source: &str, env: &[(&str, &str)]) -> String {
    printed(source, env).0
}

#[test]
fn constant_directives() {
    let source = r##"
//! Says what cfgs it sets.

/// The script.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(has_{{braces}})");
    print!("cargo:rustc-cfg=a\ncargo:rustc-cfg=b\n");
    println!(r#"cargo:rustc-env=QUOTED="x""#);
    println!("cargo:rustc-cfg=\x41\u{42}\
              C");
    println!();
    eprintln!("note",);
}
"##;
    let (out, err) = printed(source, &[]);
    assert_eq!(
        out,
        "cargo:rerun-if-changed=build.rs\ncargo::rustc-check-cfg=cfg(has_{braces})\n\
         cargo:rustc-cfg=a\ncargo:rustc-cfg=b\ncargo:rustc-env=QUOTED=\"x\"\ncargo:rustc-cfg=ABC\n\n"
    );
    assert_eq!(err, "note\n");
    assert_eq!(stdout("fn main() {}", &[]), "");
    assert_eq!(stdout("fn main() { println!(\"a\") }", &[]), "a\n");
}

#[test]
fn conditions_on_the_environment() {
    let source = r#"
use std::env;
fn main() {
    if std::env::var("ICU4X_DATA_DIR").is_ok() {
        println!("cargo:rustc-cfg=custom");
    }
    println!("cargo:rerun-if-env-changed=ICU4X_DATA_DIR");
    let sanitizers = env::var("CARGO_CFG_SANITIZE").unwrap_or_default();
    if sanitizers.contains("thread") && !::std::env::var_os("NO_TSAN").is_some() {
        println!("cargo:rustc-cfg=tsan");
    } else if sanitizers == "memory" || (env::var("A").is_err() && "x" != sanitizers) {
        println!("cargo:rustc-cfg=other");
    } else {
        println!("cargo:rustc-cfg=none");
    }
    if env::var("EMPTY").unwrap_or_default().is_empty() { println!("empty") }
}
"#;
    let tail = "cargo:rerun-if-env-changed=ICU4X_DATA_DIR\n";
    assert_eq!(stdout(source, &[]), format!("{tail}cargo:rustc-cfg=other\nempty\n"));
    assert_eq!(
        stdout(source, &[("ICU4X_DATA_DIR", ""), ("CARGO_CFG_SANITIZE", "thread,address")]),
        format!("cargo:rustc-cfg=custom\n{tail}cargo:rustc-cfg=tsan\nempty\n")
    );
    let env = [("CARGO_CFG_SANITIZE", "thread"), ("NO_TSAN", "1"), ("A", "1"), ("EMPTY", "x")];
    assert_eq!(stdout(source, &env), format!("{tail}cargo:rustc-cfg=other\n"));
    let env = [("CARGO_CFG_SANITIZE", "x"), ("A", "1")];
    assert_eq!(stdout(source, &env), format!("{tail}cargo:rustc-cfg=none\nempty\n"));
}

#[test]
fn a_let_is_scoped_to_its_block() {
    let source = r#"fn main() {
        let a = "outer";
        if true_ish() {}
    }"#;
    assert!(of(source, Edition::Edition2021).is_err());
    let source = r#"fn main() {
        let a = "outer";
        if a == "outer" { let a = std::env::var("X").unwrap_or_default(); if a == "x" { println!("inner x") } }
        if a == "outer" { println!("outer") }
    }"#;
    assert_eq!(stdout(source, &[("X", "x")]), "inner x\nouter\n");
}

#[test]
fn what_is_not_inert() {
    for source in [
        // Anything that could compile to something else, fail to compile, or panic.
        "#![deny(warnings)] fn main() {}",
        "#[cfg(unix)] fn main() {}",
        "fn main() { println!(\"{}\", 1); }",
        "fn main() { println!(\"{x}\"); }",
        "fn main() { println!(\"}\"); }",
        "fn main() { print!(); }",
        "fn main() { println!(\"a\") println!(\"b\") }",
        "fn main() { println!(\"a\");; }",
        "fn main() { if std::env::var(\"X\").is_ok() {}; }",
        "fn main() { println!(b\"a\"); }",
        "fn main() { println!(\"a\"x); }",
        "fn main() { println!{\"a\"} }",
        "fn main() { std::env::var(\"X\").unwrap(); }",
        "fn main() { if std::env::var(\"X\").unwrap() == \"a\" {} }",
        "fn main() { if std::env::var(\"\").is_ok() {} }",
        "fn main() { if std::env::var(\"A=B\").is_ok() {} }",
        "fn main() { if env::var(\"X\").is_ok() {} }",
        "fn main() { if std::env::var(\"X\").unwrap_or_default() {} }",
        "fn main() { if !std::env::var(\"X\").unwrap_or_default() == \"a\" {} }",
        "fn main() { if \"a\" == \"b\" == \"c\" {} }",
        "fn main() { if std :: env::var(\"X\").is_ok() {} }",
        "fn main() { if std::env::var(\"X\").is_ok() & &true {} }",
        "use std::env; use std::env; fn main() {}",
        "use std::process; fn main() {}",
        "use std::*; fn main() {}",
        "macro_rules! println { ($($t:tt)*) => {} } fn main() { println!(\"a\"); }",
        "mod other; fn main() {}",
        "extern crate std as env; fn main() {}",
        "fn main() { include!(\"other.rs\"); }",
        "fn main() {} fn main() {}",
        "fn helper() {} fn main() {}",
        "pub fn main() {}",
        "fn main() -> () {}",
        "fn main() { r#println!(\"a\"); }",
        "fn main() {",
        "fn main() { /* unterminated }",
        "fn main() { println!(\"a\"); /// nothing\n }",
        "fn main() {} /// nothing",
        "fn main() {} //! late",
        "/// a\n//! b\nfn main() {}",
        "#!/usr/bin/env rust\nfn main() {}",
        "fn main() { println!(\"a\r\"); }",
        "",
    ] {
        assert!(of(source, Edition::Edition2021).is_err(), "inert: {source}");
    }
}

#[test]
fn comments_that_are_not_docs() {
    for source in [
        "//// not a doc\nfn main() {} //// nor this",
        "fn main() { /**/ println!(\"a\"); /*** nor */ }",
        "\u{feff}fn main() {}\r\n",
        "/** an item's */ fn main() {}",
    ] {
        assert!(of(source, Edition::Edition2021).is_ok(), "not inert: {source}");
    }
}

#[test]
fn the_editions() {
    let source = "fn main() { if ::std::env::var(\"X\").is_ok() { println!(\"x\") } }";
    for edition in Edition::iter() {
        assert!(of(source, edition).is_ok(), "{edition}");
    }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|it| (*it).to_owned()).collect()
}

#[test]
fn what_rustc_is_given() {
    let cargo = [
        "--crate-name",
        "build_script_build",
        "--edition=2021",
        "build.rs",
        "--error-format=json",
        "--json=diagnostic-rendered-ansi,artifacts,future-incompat",
        "--diagnostic-width=100",
        "--crate-type",
        "bin",
        "--emit=dep-info,link",
        "-C",
        "embed-bitcode=no",
        "-C",
        "debuginfo=2",
        "--cfg",
        "feature=\"std\"",
        "--check-cfg",
        "cfg(docsrs,test)",
        "-C",
        "metadata=0123",
        "-C",
        "extra-filename=-0123",
        "--out-dir",
        "/t/debug/build/x-0123",
        "-L",
        "dependency=/t/debug/deps",
        "--extern",
        "cc=/t/debug/deps/libcc-1.rlib",
        "-Aunused",
        "-W",
        "missing-docs",
    ];
    assert_eq!(args(&strings(&cargo)), Ok(()));
    let with = |more: &[&str]| args(&strings(&[&cargo[..], more].concat()));
    assert!(with(&["-D", "warnings"]).is_err());
    assert!(with(&["-Dwarnings"]).is_err());
    assert!(with(&["--deny=unused"]).is_err());
    assert_eq!(with(&["-D", "warnings", "--cap-lints", "allow"]), Ok(()));
    assert_eq!(with(&["-Dwarnings", "--cap-lints=warn"]), Ok(()));
    assert!(with(&["-Dwarnings", "--cap-lints", "deny"]).is_err());
    assert!(with(&["--extern", "std=/x/libstd.rlib"]).is_err());
    assert_eq!(with(&["-C", "link-arg=-fuse-ld=mold", "-Ctarget-cpu=native"]), Ok(()));
    assert_eq!(with(&["-Dclippy::all", "--deny=rustdoc::broken"]), Ok(()));
    assert!(with(&["-Drust::unused"]).is_err());
    assert!(with(&["-C", "instrument-coverage"]).is_err());
    assert!(with(&["-lmissing"]).is_err());
    assert!(with(&["-Zcrate-attr=deny(warnings)"]).is_err());
    assert!(with(&["@args"]).is_err());
    assert!(with(&["--crate-type", "lib"]).is_err());
    assert!(with(&["--cfg"]).is_err());
}
