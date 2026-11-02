//! The cases of cargo's tests of what a script prints (`tests/testsuite/build_script.rs`,
//! `build_script_env.rs`, `build_script_extra_link_arg.rs` and `check_cfg.rs` in
//! rust-lang/cargo at 5f94df478, MIT or Apache-2.0), with the messages cargo fails with. Each
//! names the test it comes from; `[ROOT]` is `/p`.

use blaze_ids::rng::SplitMix64;

use super::*;
use crate::unit::Target;

fn target(name: &str, kind: &str, crate_type: &str) -> Target {
    Target { name: name.into(), kind: vec![kind.into()], crate_types: vec![crate_type.into()] }
}

fn foo(version: &str, rust_version: Option<&str>, targets: Vec<Target>) -> Package {
    Package {
        name: "foo".into(),
        version: version.into(),
        descr: Some(format!("foo v{version} (/p/foo)")),
        rust_version: rust_version.map(str::to_owned),
        targets,
        ..Package::default()
    }
}

fn lib() -> Vec<Target> {
    vec![target("foo", "lib", "lib")]
}

const OUT: &str = "/p/foo/target/debug/build/foo-1/out";

fn read(package: &Package, printed: &str) -> Result<Output, Error> {
    let cx = Context {
        package,
        out_dir_when_generated: Path::new(OUT),
        out_dir: Path::new(OUT),
        nightly_features_allowed: false,
        rustc_bootstrap: None,
    };
    parse(printed.as_bytes(), &cx)
}

fn error(package: &Package, printed: &str) -> String {
    read(package, printed).unwrap_err().0
}

/// `custom_build_script_wrong_rustc_flags`.
#[test]
fn rustc_flags_are_l_and_capital_l_only() {
    let package = foo("0.5.0", None, lib());
    assert_eq!(
        error(&package, "cargo::rustc-flags=-aaa -bbb"),
        "only `-l` and `-L` flags are allowed in build script of `foo v0.5.0 (/p/foo)`: `-aaa -bbb`"
    );
}

/// `custom_build_script_rustc_flags` and `custom_build_script_rustc_flags_no_space`.
#[test]
fn rustc_flags() {
    let package = foo("0.5.0", None, lib());
    for printed in [
        "cargo::rustc-flags=-l nonexistinglib -L /p/foo/dummy-path1 -L /p/foo/dummy-path2",
        "cargo::rustc-flags=-lnonexistinglib -L/p/foo/dummy-path1 -L/p/foo/dummy-path2",
    ] {
        let out = read(&package, printed).unwrap();
        assert_eq!(out.library_links, ["nonexistinglib"]);
        assert_eq!(
            out.library_paths,
            [
                LibraryPath::External("/p/foo/dummy-path1".into()),
                LibraryPath::External("/p/foo/dummy-path2".into())
            ]
        );
    }
    assert_eq!(
        error(&package, "cargo:rustc-flags=-L"),
        "flag in rustc-flags has no value in build script of `foo v0.5.0 (/p/foo)`: "
    );
}

/// `output_separate_lines` and `output_separate_lines_new`.
#[test]
fn link_search_and_libs() {
    let package = foo("0.5.0", None, lib());
    let printed = "cargo::rustc-link-search=foo\ncargo::rustc-link-lib=static=foo\n\
                   cargo::rustc-link-lib=bar\ncargo::rustc-link-search=bar\n";
    let out = read(&package, printed).unwrap();
    assert_eq!(out.library_links, ["static=foo", "bar"]);
    let paths: Vec<_> = out.library_paths.iter().map(LibraryPath::path).collect();
    assert_eq!(paths, [Path::new("foo"), Path::new("bar")]);
    let old =
        read(&package, "cargo:rustc-flags=-L foo\ncargo:rustc-flags=-l static=foo\n").unwrap();
    assert_eq!(old.library_links, ["static=foo"]);
}

/// `wrong_output`.
#[test]
fn a_new_line_without_a_value() {
    let package = foo("0.0.1", None, lib());
    assert_eq!(
        error(&package, "cargo::example"),
        "invalid output in build script of `foo v0.0.1 (/p/foo)`: `cargo::example`\n\
         Expected a line with `cargo::KEY=VALUE` with an `=` character, but none was found.\n\
         See https://doc.rust-lang.org/cargo/reference/build-scripts.html#outputs-of-the-build-script \
         for more information about build script outputs."
    );
}

/// `test_old_syntax`: an unknown key of the old syntax is metadata.
#[test]
fn old_syntax_metadata() {
    let package = foo("0.0.1", None, lib());
    let out =
        read(&package, "cargo:rustc-env=FOO=foo\ncargo:foo=foo\ncargo:error=not one\n").unwrap();
    assert_eq!(out.env, [("FOO".to_owned(), "foo".to_owned())]);
    assert_eq!(
        out.metadata,
        [("foo".to_owned(), "foo".to_owned()), ("error".to_owned(), "not one".to_owned())]
    );
    assert!(out.log_messages.is_empty());
    let out = read(&package, "cargo:metadata=a=b\n").unwrap();
    assert_eq!(out.metadata, [("metadata".to_owned(), "a=b".to_owned())]);
}

/// `test_invalid_old_syntax` and `test_invalid_new_syntax`.
#[test]
fn invalid_metadata() {
    let package = foo("0.0.1", None, lib());
    assert_eq!(
        error(&package, "cargo:foo"),
        "invalid output in build script of `foo v0.0.1 (/p/foo)`: `cargo:foo`\n\
         Expected a line with `cargo:KEY=VALUE` with an `=` character, but none was found.\n\
         See https://doc.rust-lang.org/cargo/reference/build-scripts.html#outputs-of-the-build-script \
         for more information about build script outputs."
    );
    assert_eq!(
        error(&package, "cargo::metadata=foo"),
        "invalid output in build script of `foo v0.0.1 (/p/foo)`: `cargo::metadata=foo`\n\
         Expected a line with `cargo::metadata=KEY=VALUE` with an `=` character, but none was found.\n\
         See https://doc.rust-lang.org/cargo/reference/build-scripts.html#outputs-of-the-build-script \
         for more information about build script outputs."
    );
    assert_eq!(
        error(&package, "cargo::foo=bar"),
        "invalid output in build script of `foo v0.0.1 (/p/foo)`: `cargo::foo=bar`\n\
         Unknown key: `foo`.\n\
         See https://doc.rust-lang.org/cargo/reference/build-scripts.html#outputs-of-the-build-script \
         for more information about build script outputs."
    );
}

/// `test_new_syntax_with_old_msrv`, `..._and_reserved_prefix`, `..._and_unknown_prefix`,
/// `test_new_syntax_with_compatible_partial_msrv` and `test_old_syntax_with_old_msrv`.
#[test]
fn new_syntax_and_rust_version() {
    let old = foo("0.5.0", Some("1.60.0"), lib());
    let first = "the `cargo::` syntax for build script output instructions was added in Rust \
                 1.77.0, but the minimum supported Rust version of `foo v0.5.0 (/p/foo)` is 1.60.0.\n";
    let docs = "See https://doc.rust-lang.org/cargo/reference/build-scripts.html#outputs-of-the-build-script \
                for more information about build script outputs.";
    assert_eq!(
        error(&old, "cargo::metadata=foo=bar"),
        format!(
            "{first}Switch to the old `cargo:foo=bar` syntax instead of `cargo::metadata=foo=bar` \
             (note the single colon).\n{docs}"
        )
    );
    assert_eq!(
        error(&old, "cargo::rustc-check-cfg=cfg(foo)"),
        format!(
            "{first}Switch to the old `cargo:rustc-check-cfg=cfg(foo)` syntax (note the single colon).\n{docs}"
        )
    );
    assert_eq!(error(&old, "cargo::foo=bar"), format!("{first}{docs}"));
    assert_eq!(read(&old, "cargo:foo=bar").unwrap().metadata, [("foo".into(), "bar".into())]);
    for compatible in ["1.77", "1.77.0", "1.80", "1.99.0"] {
        let package = foo("0.0.0", Some(compatible), lib());
        assert!(read(&package, "cargo::metadata=foo=bar").is_ok(), "{compatible}");
    }
    for older in ["1", "1.76", "1.76.9"] {
        let package = foo("0.0.0", Some(older), lib());
        let message = error(&package, "cargo::metadata=foo=bar");
        assert!(
            message.contains(&format!("Rust version of `foo v0.0.0 (/p/foo)` is {older}.")),
            "{message}"
        );
    }
    // A `rust-version` that is no version is refused, not taken to allow `cargo::`.
    for unread in ["", "1.x", "1.77.0.1", "latest"] {
        let package = foo("0.0.0", Some(unread), lib());
        let want = crate::unit::Unfilled::RustVersion {
            package: "foo v0.0.0 (/p/foo)".into(),
            rust_version: unread.into(),
        };
        assert_eq!(read(&package, "cargo::metadata=foo=bar"), Err(Error::from(want)), "{unread}");
        assert!(read(&package, "cargo:foo=bar").is_ok(), "the old syntax reads no rust-version");
    }
}

/// `build_script_extra_link_arg_bin`, `..._bin_single`, `..._tests`, `..._benches`,
/// `..._examples`, `link_arg_missing_target` and `cdylib_both_forms`.
#[test]
fn link_args() {
    let package = foo(
        "0.0.1",
        None,
        vec![
            target("foo", "lib", "lib"),
            target("foo", "bin", "bin"),
            target("t", "test", "bin"),
            target("b", "bench", "bin"),
            target("e", "example", "bin"),
        ],
    );
    let printed = "cargo::rustc-link-arg=all\ncargo::rustc-link-arg-bins=bins\n\
                   cargo::rustc-link-arg-bin=foo=one\ncargo:rustc-link-arg-tests=tests\n\
                   cargo::rustc-link-arg-benches=benches\ncargo::rustc-link-arg-examples=examples\n";
    let out = read(&package, printed).unwrap();
    assert_eq!(
        out.linker_args,
        [
            (LinkArgTarget::All, "all".to_owned()),
            (LinkArgTarget::Bin, "bins".to_owned()),
            (LinkArgTarget::SingleBin("foo".to_owned()), "one".to_owned()),
            (LinkArgTarget::Test, "tests".to_owned()),
            (LinkArgTarget::Bench, "benches".to_owned()),
            (LinkArgTarget::Example, "examples".to_owned()),
        ]
    );
    let lib_only = foo("0.0.1", None, lib());
    assert_eq!(
        error(&lib_only, "cargo::rustc-link-arg-bins=--bogus"),
        "invalid instruction `cargo::rustc-link-arg-bins` from build script of `foo v0.0.1 (/p/foo)`\n\
         The package foo v0.0.1 (/p/foo) does not have a bin target."
    );
    assert_eq!(
        error(&lib_only, "cargo::rustc-link-arg-bin=abc=--bogus"),
        "invalid instruction `cargo::rustc-link-arg-bin` from build script of `foo v0.0.1 (/p/foo)`\n\
         The package foo v0.0.1 (/p/foo) does not have a bin target with the name `abc`."
    );
    assert_eq!(
        error(&lib_only, "cargo::rustc-link-arg-bin=abc"),
        "invalid instruction `cargo::rustc-link-arg-bin=abc` from build script of `foo v0.0.1 (/p/foo)`\n\
         The instruction should have the form cargo::rustc-link-arg-bin=BIN=ARG"
    );
    assert!(
        error(&lib_only, "cargo:rustc-link-arg-tests=x").contains("does not have a test target")
    );
    assert!(
        error(&lib_only, "cargo::rustc-link-arg-benches=x")
            .contains("does not have a benchmark target")
    );
    assert!(
        error(&lib_only, "cargo::rustc-link-arg-examples=x")
            .contains("does not have a example target")
    );
    // A cdylib argument of a package without one is kept, with a warning.
    let out = read(&lib_only, "cargo::rustc-cdylib-link-arg=a\ncargo::rustc-link-arg-cdylib=b\n")
        .unwrap();
    assert_eq!(
        out.linker_args,
        [(LinkArgTarget::Cdylib, "a".to_owned()), (LinkArgTarget::Cdylib, "b".to_owned())]
    );
    assert_eq!(out.log_messages.len(), 2);
    assert!(out.log_messages[0].1.starts_with(
        "cargo::rustc-cdylib-link-arg was specified in the build script of foo v0.0.1 (/p/foo), \
         but that package does not contain a cdylib target"
    ));
    let cdylib = foo("0.0.1", None, vec![target("foo", "cdylib", "cdylib")]);
    assert!(read(&cdylib, "cargo::rustc-cdylib-link-arg=a").unwrap().log_messages.is_empty());
}

/// `rustc_bootstrap`.
#[test]
fn rustc_bootstrap() {
    let package = Package {
        name: "has-dashes".into(),
        version: "0.0.1".into(),
        descr: Some("has-dashes v0.0.1 (/p/foo)".into()),
        targets: vec![target("has-dashes", "lib", "lib")],
        ..Package::default()
    };
    let printed = b"cargo::rustc-env=RUSTC_BOOTSTRAP=1";
    let note = "cannot set `RUSTC_BOOTSTRAP=1` from build script of `has-dashes v0.0.1 (/p/foo)`.\n\
                note: crates cannot set `RUSTC_BOOTSTRAP` themselves, as doing so would subvert \
                the stability guarantees of Rust for your project.";
    let with = |nightly, env: Option<&str>| {
        let cx = Context {
            package: &package,
            out_dir_when_generated: Path::new(OUT),
            out_dir: Path::new(OUT),
            nightly_features_allowed: nightly,
            rustc_bootstrap: env.map(str::to_owned),
        };
        parse(printed, &cx)
    };
    assert_eq!(
        with(false, None).unwrap_err().0,
        format!(
            "{note}\nhelp: If you're sure you want to do this in your project, set the environment \
             variable `RUSTC_BOOTSTRAP=has_dashes` before running cargo instead."
        )
    );
    let warned = with(true, Some("1")).unwrap();
    assert_eq!(warned.log_messages, [(Severity::Warning, note.to_owned())]);
    assert!(warned.env.is_empty());
    assert!(with(false, Some("has_dashes")).is_ok());
    assert!(with(false, Some("bar")).is_err());
    let unit = Unit { package: package.clone(), ..Unit::default() };
    let cx = Context::of(&unit, &[("RUSTC_BOOTSTRAP".into(), "1".into())]);
    assert!(cx.nightly_features_allowed);
    assert!(!Context::of(&unit, &[]).nightly_features_allowed);
}

/// `warnings_emitted`, `errors_and_warnings_emitted_and_build_failed`, `non_utf8_output`,
/// `rustc_cfg_with_and_without_value`, `rerun_if_env_changes` and the rest of what is kept
/// as printed.
#[test]
fn the_rest() {
    let package = foo("0.0.1", None, lib());
    let mut printed = b"cargo::warning=foo\ncargo:warning=bar\n\xff\xff\n  cargo::rustc-cfg=foo  \n\
                        cargo::rustc-cfg=feature=\"x\"\ncargo::rustc-check-cfg=cfg(foo, values(\"a\"))\n\
                        cargo::rustc-env=A=b=c\ncargo::error=bad\ncargo::rerun-if-changed=src/x\n\
                        cargo::rerun-if-env-changed=FOO\ncargo::metadata=k=v=w\nnot cargo:x\n"
        .to_vec();
    printed.extend(format!("cargo::rustc-link-search=native={OUT}/lib\n").bytes());
    let cx = Context {
        package: &package,
        out_dir_when_generated: Path::new("/old/out"),
        out_dir: Path::new("/new/out"),
        nightly_features_allowed: false,
        rustc_bootstrap: None,
    };
    let moved =
        parse(b"cargo::rustc-link-search=/old/out/lib\ncargo::rustc-link-search=/elsewhere", &cx)
            .unwrap();
    assert_eq!(
        moved.library_paths,
        [
            LibraryPath::CargoArtifact("/new/out/lib".into()),
            LibraryPath::External("/elsewhere".into())
        ]
    );
    let out = parse(
        &printed,
        &Context { out_dir_when_generated: Path::new(OUT), out_dir: Path::new(OUT), ..cx },
    )
    .unwrap();
    assert_eq!(
        out.log_messages,
        [
            (Severity::Warning, "foo".to_owned()),
            (Severity::Warning, "bar".to_owned()),
            (Severity::Error, "bad".to_owned())
        ]
    );
    assert_eq!(out.cfgs, ["foo", "feature=\"x\""]);
    assert_eq!(out.check_cfgs, ["cfg(foo, values(\"a\"))"]);
    assert_eq!(out.env, [("A".to_owned(), "b=c".to_owned())]);
    assert_eq!(out.rerun_if_changed, [PathBuf::from("src/x")]);
    assert_eq!(out.rerun_if_env_changed, ["FOO"]);
    assert_eq!(out.metadata, [("k".to_owned(), "v=w".to_owned())]);
    assert_eq!(out.library_paths, [LibraryPath::CargoArtifact(format!("native={OUT}/lib").into())]);
    assert_eq!(
        error(&package, "cargo::rustc-env=NOVALUE"),
        "Variable rustc-env has no value in build script of `foo v0.0.1 (/p/foo)`: NOVALUE"
    );
}

/// A directive after any of Unicode's spaces is read, as `str::trim` reads it: one past
/// ASCII takes the whole check, one in ASCII (the vertical tab too) the quick one.
#[test]
fn a_directive_after_any_space_is_read() {
    let package = foo("0.0.1", None, lib());
    let out = read(
        &package,
        "\u{a0}cargo:rustc-cfg=a\n\x0b\x0ccargo::rustc-cfg=b\r\n\u{2003}\tcargo:rustc-cfg=c\n\u{85}x\n",
    )
    .unwrap();
    assert_eq!(out.cfgs, ["a", "b", "c"]);
}

/// [`directive`] against what cargo does to every line (`from_utf8`, `trim`, then the prefix),
/// over seeded lines of the bytes that matter: ASCII and Unicode spaces, `cargo:`'s pieces,
/// bytes that are not UTF-8.
#[test]
fn a_line_is_a_directive_exactly_when_cargo_reads_one() {
    let pieces: [&[u8]; 14] = [
        b" ", b"\t", b"\x0b", b"\x0c", b"\r", "\u{a0}".as_bytes(), "\u{85}".as_bytes(),
        "\u{2003}".as_bytes(), b"cargo:", b"cargo", b":", b"x=y", b"\xff", b"\xc2",
    ];
    fn cargo(line: &[u8]) -> Option<&str> {
        std::str::from_utf8(line).ok().map(str::trim).filter(|it| it.starts_with("cargo:"))
    }
    let mut rng = SplitMix64::new(0x853c_49e6_748f_ea9b);
    for _ in 0..50_000 {
        let mut line = Vec::new();
        let state = rng.next_u64();
        for at in 0..(state % 6) {
            line.extend_from_slice(pieces[((state >> (8 + 4 * at)) % pieces.len() as u64) as usize]);
        }
        assert_eq!(directive(&line), cargo(&line), "{line:?}");
    }
}

/// The `match` of reserved keys against cargo's list of prefixes.
#[test]
fn a_reserved_key_is_one_of_cargo_s_prefixes() {
    const CARGO: &[&str] = &[
        "rustc-flags=", "rustc-link-lib=", "rustc-link-search=", "rustc-link-arg-cdylib=",
        "rustc-cdylib-link-arg=", "rustc-link-arg-bins=", "rustc-link-arg-bin=",
        "rustc-link-arg-tests=", "rustc-link-arg-benches=", "rustc-link-arg-examples=",
        "rustc-link-arg=", "rustc-cfg=", "rustc-check-cfg=", "rustc-env=", "warning=",
        "rerun-if-changed=", "rerun-if-env-changed=",
    ];
    let mut flags: Vec<String> = Vec::new();
    for prefix in CARGO {
        let key = &prefix[..prefix.len() - 1];
        flags.extend([prefix.to_string(), format!("{prefix}x=y"), key.to_owned(), format!("{key}x=1")]);
        flags.push(format!("{}={key}", &key[..key.len() - 1]));
    }
    flags.extend(["".to_owned(), "=".to_owned(), "metadata=k=v".to_owned(), "error=e".to_owned()]);
    for flag in &flags {
        assert_eq!(has_reserved_prefix(flag), CARGO.iter().any(|it| flag.starts_with(it)), "{flag:?}");
    }
}

#[test]
fn link_lib_specs() {
    let lib = LinkLib::parse("static:+whole-archive,-bundle=foo:bar");
    assert_eq!(lib.kind, Some("static"));
    assert_eq!(lib.modifiers, [(true, "whole-archive"), (false, "bundle")]);
    assert_eq!((lib.name, lib.rename), ("foo", Some("bar")));
    assert!(lib.has("whole-archive") && !lib.has("bundle") && !lib.has("verbatim"));
    let plain = LinkLib::parse("z");
    assert_eq!((plain.kind, plain.name, plain.rename), (None, "z", None));
    assert_eq!(LinkLib::parse("framework=Security").kind, Some("framework"));
}

/// [`Key::of`]'s length-then-bytes match against a search of every name, over each name,
/// each with one byte changed, cut or lengthened, and seeded bytes of every length up to 24.
#[test]
fn a_key_is_found_exactly_by_its_name() {
    let searched = |name: &[u8]| KEYS.iter().position(|it| it.as_bytes() == name);
    let mut names: Vec<Vec<u8>> = Vec::new();
    for key in KEYS {
        let key = key.as_bytes();
        names.push(key.to_vec());
        names.push(key[..key.len() - 1].to_vec());
        names.push([key, b"s"].concat());
        for at in 0..key.len() {
            for byte in [b'-', b'a', b'e', b'l', b'u', b'c', key[at] ^ 0x20] {
                let mut changed = key.to_vec();
                changed[at] = byte;
                names.push(changed);
            }
        }
    }
    let mut rng = SplitMix64::new(0x2545_f491_4f6c_dd1d);
    for _ in 0..20_000 {
        let state = rng.next_u64();
        let len = (state % 25) as usize;
        names.push(
            (0..len).map(|at| b"rustc-linkargbe"[(state >> (at % 60)) as usize % 15]).collect(),
        );
    }
    for name in &names {
        let found = Key::of(name);
        assert_eq!(
            found.map(|it| it as usize),
            searched(name),
            "{:?}",
            String::from_utf8_lossy(name)
        );
        if let Some(key) = found {
            assert_eq!(key.name().as_bytes(), name.as_slice());
        }
    }
}

/// The single borrowed pass against the reading it replaced ([`reference::parse`]), over
/// seeded outputs of directives of every key, of both syntaxes, valid or not, for packages
/// with and without each kind of target and an old `rust-version`, with the `OUT_DIR`
/// moved or not: the same [`Output`], or the same error.
#[test]
fn the_single_pass_reads_as_the_old_parser() {
    let prefixes = ["cargo:", "cargo::", "  cargo:", "\u{a0}cargo::", "\tcargo::", "x", "cargo"];
    let mut keys: Vec<&str> = KEYS.to_vec();
    keys.extend(["foo", "", "RUSTC", "rustc-link", "warnings"]);
    let values = [
        "a",
        "x=y",
        "-l z -L native=/old/out/lib",
        "-lfoo   -L/elsewhere",
        "-L",
        "-q x",
        "/old/out/lib",
        "native=/old/out/x/old/out",
        "RUSTC_BOOTSTRAP=1",
        "FOO=bar=baz",
        "foo=one",
        "abc=two",
        "noeq",
        " spaced  \t",
        "\u{e9}t\u{e9}",
        "",
        "cfg(foo, values(\"a\"))",
    ];
    let ends: [&[u8]; 4] = [b"\n", b"\r\n", b"\xff\n", b"\n\n"];
    let targets = [
        lib(),
        vec![
            target("foo", "lib", "lib"),
            target("foo", "bin", "bin"),
            target("t", "test", "bin"),
            target("b", "bench", "bin"),
            target("e", "example", "bin"),
        ],
        vec![target("foo", "cdylib", "cdylib")],
    ];
    let packages: Vec<Package> = targets
        .iter()
        .flat_map(|targets| {
            [None, Some("1.60"), Some("1.80.0")].map(|msrv| foo("0.1.0", msrv, targets.clone()))
        })
        .collect();
    let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
    let (mut agreed_ok, mut agreed_err) = (0, 0);
    for _ in 0..40_000 {
        let mut printed = Vec::new();
        for _ in 0..1 + rng.index(5) {
            printed.extend_from_slice(prefixes[rng.index(prefixes.len())].as_bytes());
            printed.extend_from_slice(keys[rng.index(keys.len())].as_bytes());
            if rng.index(6) > 0 {
                printed.push(b'=');
            }
            printed.extend_from_slice(values[rng.index(values.len())].as_bytes());
            printed.extend_from_slice(ends[rng.index(ends.len())]);
        }
        let package = &packages[rng.index(packages.len())];
        let generated = if rng.index(2) == 0 { "/old/out" } else { "/new/out" };
        let rustc_bootstrap = [None, Some("1"), Some("foo")][rng.index(3)].map(str::to_owned);
        let cx = Context {
            package,
            out_dir_when_generated: Path::new(generated),
            out_dir: Path::new("/new/out"),
            nightly_features_allowed: rng.index(2) == 0,
            rustc_bootstrap,
        };
        let (new, old) = (parse(&printed, &cx), reference::parse(&printed, &cx));
        assert_eq!(new, old, "{:?}", String::from_utf8_lossy(&printed));
        match new {
            Ok(_) => agreed_ok += 1,
            Err(_) => agreed_err += 1,
        }
    }
    assert!(agreed_ok > 1_000 && agreed_err > 1_000, "{agreed_ok} read, {agreed_err} refused");
}

/// A value is the output's own bytes, borrowed, unless it names the `OUT_DIR` the script ran
/// with and that moved; the pieces of `rustc-flags`, `rustc-env` and metadata too.
#[test]
fn a_value_borrows_the_output_unless_its_out_dir_moved() {
    let package = foo("0.0.1", None, lib());
    let printed = b"cargo::rustc-cfg=a\ncargo:rustc-flags=-l z -L /old/out/lib\n\
                    cargo::rustc-env=K=V\ncargo:k=v\ncargo::rustc-link-search=/old/out/x\n";
    let read_in = |generated: &str| {
        let cx = Context {
            package: &package,
            out_dir_when_generated: Path::new(generated),
            out_dir: Path::new("/new/out"),
            nightly_features_allowed: false,
            rustc_bootstrap: None,
        };
        let mut directives: Vec<Directive<'_>> = Vec::new();
        super::read(printed, &cx, &mut directives).unwrap();
        directives
    };
    let borrowed = |it: &Cow<'_, str>| matches!(it, Cow::Borrowed(_));
    let same = read_in("/new/out");
    assert_eq!(same.len(), 6);
    for directive in &same {
        let all_borrowed = match directive {
            Directive::Cfg(it) | Directive::LinkLib(it) | Directive::LinkSearch(it) => borrowed(it),
            Directive::Env(a, b) | Directive::Metadata(a, b) => borrowed(a) && borrowed(b),
            other => panic!("{other:?}"),
        };
        assert!(all_borrowed, "{directive:?}");
    }
    let moved = read_in("/old/out");
    assert_eq!(moved[0], Directive::Cfg(Cow::Borrowed("a")));
    assert_eq!(moved[2], Directive::LinkSearch(Cow::Owned("/new/out/lib".to_owned())));
    assert!(matches!(&moved[5], Directive::LinkSearch(Cow::Owned(it)) if it == "/new/out/x"));
}

/// [`read`] of every directive that names no moved `OUT_DIR`, no single bin and no message
/// it makes allocates nothing: each value is borrowed from what was printed.
#[test]
fn read_allocates_nothing() {
    struct Count(usize);
    impl<'a> Sink<'a> for Count {
        fn directive(&mut self, _: Directive<'a>) {
            self.0 += 1;
        }
    }
    let package = foo("0.5.0", None, lib());
    let cx = Context {
        package: &package,
        out_dir_when_generated: Path::new(OUT),
        out_dir: Path::new(OUT),
        nightly_features_allowed: false,
        rustc_bootstrap: None,
    };
    let printed = "cargo:rustc-link-lib=static=foo\n\
                   cargo:rustc-link-search=native=/p/foo/target/debug/build/foo-1/out/lib\n\
                   cargo:rustc-cfg=has_foo\n\
                   cargo:rustc-check-cfg=cfg(has_foo)\n\
                   cargo:rustc-env=FOO=bar\n\
                   cargo:rustc-flags=-l z -L /usr/lib\n\
                   cargo:rustc-link-arg=-Wl,-z,now\n\
                   cargo:warning=hello\r\n\
                   not a directive\n\
                   cargo:rerun-if-changed=build.rs\n\
                   cargo:rerun-if-env-changed=CC\n\
                   cargo:include=/p/foo/include";
    let mut count = Count(0);
    blaze_alloc_count::assert_at_most(0, "reading a script's output", || {
        super::read(printed.as_bytes(), &cx, &mut count).unwrap()
    });
    assert_eq!(count.0, 12);
}

mod reference;
