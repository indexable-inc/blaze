//! What a build script (user code) prints on its standard output, read as cargo reads it:
//! `blaze_build_script::output::parse(input, cx)`, under three contexts (a library alone with
//! nothing moved; every kind of target, an old `rust-version`, a moved `OUT_DIR` and nightly
//! features; a `rust-version` with `cargo::` and an empty generated `OUT_DIR`), one a run.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - A line is a directive exactly when cargo reads one (`from_utf8`, `trim`, then `cargo:`,
//!   the model of `a_line_is_a_directive_exactly_when_cargo_reads_one`): the input reads the
//!   same as its directives alone, each trimmed and ended.
//! - Each line is read on its own: the input reads as its directives read one at a time, their
//!   outputs added up in order, to the first error, which is the input's.
//! - What is kept has no line break, but in a message cargo words, and with a moved `OUT_DIR`
//!   names no path in the old one; a library path is cargo's own exactly when it is in the
//!   `OUT_DIR`.
//! - Reading asks for at most `blaze_fuzz::allowed(len)` bytes at once.

#![forbid(unsafe_code)]

use std::{path::Path, sync::OnceLock, time::Duration};

use blaze_build_script::{
    Error,
    output::{Context, LibraryPath, LinkArgTarget, Output, parse, search_path_of},
    unit::{Package, Target as UnitTarget},
};
use blaze_fuzzer::Target;

const TARGET: Target = Target {
    name: "usercode_script_output",
    check,
    seeds,
    dictionary: &[
        b"cargo:",
        b"cargo::",
        b"\n",
        b"\r\n",
        b"=",
        b"rustc-flags=",
        b"rustc-link-lib=",
        b"rustc-link-search=",
        b"rustc-link-arg-cdylib=",
        b"rustc-cdylib-link-arg=",
        b"rustc-link-arg-bins=",
        b"rustc-link-arg-bin=",
        b"rustc-link-arg-tests=",
        b"rustc-link-arg-benches=",
        b"rustc-link-arg-examples=",
        b"rustc-link-arg=",
        b"rustc-cfg=",
        b"rustc-check-cfg=",
        b"rustc-env=",
        b"RUSTC_BOOTSTRAP=",
        b"warning=",
        b"error=",
        b"metadata=",
        b"rerun-if-changed=",
        b"rerun-if-env-changed=",
        b"-l",
        b"-L",
        b"native=",
        b"/old/out",
        b"/p/foo/out",
        b"\xc2\xa0",
        b"\xe2\x80\x83",
        b"\x0b",
        b"\xff",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(300),
};

const OUT: &str = "/p/foo/out";

fn target(name: &str, kind: &str, crate_type: &str) -> UnitTarget {
    UnitTarget { name: name.into(), kind: vec![kind.into()], crate_types: vec![crate_type.into()] }
}

/// A package and the rest of what its output is read with.
struct Case {
    package: Package,
    generated: &'static str,
    out_dir: &'static str,
    nightly: bool,
    rustc_bootstrap: Option<String>,
}

impl Case {
    fn cx(&self) -> Context<'_> {
        Context {
            package: &self.package,
            out_dir_when_generated: Path::new(self.generated),
            out_dir: Path::new(self.out_dir),
            nightly_features_allowed: self.nightly,
            rustc_bootstrap: self.rustc_bootstrap.clone(),
        }
    }
}

/// The contexts an output is read under, one a run (`blaze_fuzzer::case`), built once; the
/// plain test takes every one of each seed.
const CONTEXTS: usize = 3;

fn cases() -> &'static [Case; CONTEXTS] {
    static CASES: OnceLock<[Case; CONTEXTS]> = OnceLock::new();
    CASES.get_or_init(made)
}

fn made() -> [Case; CONTEXTS] {
    let package = |name: &str, rust_version: Option<&str>, targets| Package {
        name: name.into(),
        version: "0.1.0".into(),
        rust_version: rust_version.map(str::to_owned),
        targets,
        ..Package::default()
    };
    [
        Case {
            package: package("foo", None, vec![target("foo", "lib", "lib")]),
            generated: OUT,
            out_dir: OUT,
            nightly: false,
            rustc_bootstrap: None,
        },
        Case {
            package: package(
                "foo",
                Some("1.60.0"),
                vec![
                    target("foo", "cdylib", "cdylib"),
                    target("foo", "bin", "bin"),
                    target("t", "test", "bin"),
                    target("b", "bench", "bin"),
                    target("e", "example", "bin"),
                ],
            ),
            generated: "/old/out",
            out_dir: "/new/out/moved",
            nightly: true,
            rustc_bootstrap: Some("1".into()),
        },
        Case {
            package: package("has-dashes", Some("1.77"), vec![target("has-dashes", "lib", "lib"), target("foo", "bin", "bin")]),
            generated: "",
            out_dir: OUT,
            nightly: false,
            rustc_bootstrap: Some("has_dashes".into()),
        },
    ]
}

/// The lines cargo reads as directives: the input split at `\n`, each line `from_utf8`,
/// `trim`, then kept when it starts `cargo:`.
fn directives(input: &[u8]) -> Vec<&str> {
    input
        .split(|it| *it == b'\n')
        .filter_map(|line| std::str::from_utf8(line).ok().map(str::trim).filter(|it| it.starts_with("cargo:")))
        .collect()
}

/// `from` added after what `into` has, field by field.
fn add(into: &mut Output, from: Output) {
    let Output {
        library_paths,
        library_links,
        linker_args,
        cfgs,
        check_cfgs,
        env,
        metadata,
        rerun_if_changed,
        rerun_if_env_changed,
        log_messages,
    } = from;
    into.library_paths.extend(library_paths);
    into.library_links.extend(library_links);
    into.linker_args.extend(linker_args);
    into.cfgs.extend(cfgs);
    into.check_cfgs.extend(check_cfgs);
    into.env.extend(env);
    into.metadata.extend(metadata);
    into.rerun_if_changed.extend(rerun_if_changed);
    into.rerun_if_env_changed.extend(rerun_if_env_changed);
    into.log_messages.extend(log_messages);
}

/// Every text kept from the script's lines (not the messages cargo words around them).
fn kept(out: &Output) -> Vec<&str> {
    let mut kept: Vec<&str> = Vec::new();
    kept.extend(out.library_paths.iter().map(|it| it.path().to_str().expect("a kept path was text")));
    kept.extend(out.library_links.iter().map(String::as_str));
    for (target, arg) in &out.linker_args {
        if let LinkArgTarget::SingleBin(bin) = target {
            kept.push(bin);
        }
        kept.push(arg);
    }
    kept.extend(out.cfgs.iter().chain(&out.check_cfgs).chain(&out.rerun_if_env_changed).map(String::as_str));
    for (name, value) in out.env.iter().chain(&out.metadata) {
        kept.extend([name.as_str(), value.as_str()]);
    }
    kept.extend(out.rerun_if_changed.iter().map(|it| it.to_str().expect("a kept path was text")));
    kept
}

fn check(data: &[u8]) {
    check_case(data, blaze_fuzzer::case(data, CONTEXTS));
}

fn check_case(data: &[u8], case: usize) {
    let lines = directives(data);
    let mut normalized = Vec::with_capacity(data.len() + 1);
    for line in &lines {
        normalized.extend_from_slice(line.as_bytes());
        normalized.push(b'\n');
    }
    let case = &cases()[case];
    let cx = case.cx();
    let (most, got) = blaze_fuzz::largest_allocation(|| parse(data, &cx));
    assert!(most <= blaze_fuzz::allowed(data.len()), "reading {} bytes asked for {most} at once", data.len());

    assert_eq!(parse(&normalized, &cx), got, "the input reads as its directives alone");

    let mut one_by_one: Result<Output, Error> = Ok(Output::default());
    for line in &lines {
        match parse(line.as_bytes(), &cx) {
            Ok(out) => add(one_by_one.as_mut().expect("an output until an error"), out),
            Err(error) => {
                one_by_one = Err(error);
                break;
            }
        }
    }
    assert_eq!(one_by_one, got, "the input reads as its directives read one at a time");

    let Ok(out) = got else { return };
    let moved = !case.generated.is_empty() && case.generated != case.out_dir;
    for text in kept(&out) {
        assert!(!text.contains('\n'), "a kept text has a line break: {text:?}");
        assert!(!(moved && text.contains(case.generated)), "a kept text names the old `OUT_DIR`: {text:?}");
    }
    for path in &out.library_paths {
        let ours = search_path_of(path.path()).starts_with(case.out_dir);
        assert_eq!(matches!(path, LibraryPath::CargoArtifact(_)), ours, "{path:?} is cargo's own exactly when in the `OUT_DIR`");
    }
}

/// What scripts print, each form cargo's tests show, with lines of a script's own between.
fn seeds() -> Vec<Vec<u8>> {
    let texts: [&[u8]; 9] = [
        b"cargo:rerun-if-changed=build.rs\ncargo::rustc-check-cfg=cfg(foo)\ncargo:rustc-cfg=foo\ncargo:links=1\n",
        b"cargo::rustc-flags=-l nonexistinglib -L /p/foo/dummy-path1 -L/p/foo/dummy-path2\ncargo:rustc-flags=-L\n",
        b"cargo::rustc-link-search=foo\ncargo::rustc-link-lib=static=foo\ncargo::rustc-link-lib=bar\n\
          cargo::rustc-link-search=native=/p/foo/out/lib\ncargo::rustc-link-search=/old/out/lib\n",
        b"cargo::rustc-link-arg=all\ncargo::rustc-link-arg-bins=bins\ncargo::rustc-link-arg-bin=foo=one\n\
          cargo:rustc-link-arg-tests=tests\ncargo::rustc-link-arg-benches=benches\n\
          cargo::rustc-link-arg-examples=examples\ncargo::rustc-cdylib-link-arg=a\ncargo::rustc-link-arg-cdylib=b\n",
        b"cargo::warning=foo\ncargo:warning=bar\n\xff\xff\n  cargo::rustc-cfg=foo  \n\
          cargo::rustc-cfg=feature=\"x\"\ncargo::rustc-check-cfg=cfg(foo, values(\"a\"))\n\
          cargo::rustc-env=A=b=c\ncargo::error=bad\ncargo::rerun-if-changed=src/x\n\
          cargo::rerun-if-env-changed=FOO\ncargo::metadata=k=v=w\nnot cargo:x\n",
        b"cargo:rustc-env=FOO=foo\ncargo:foo=foo\ncargo:error=not one\ncargo:metadata=a=b\ncargo:foo\n",
        b"cargo::rustc-env=RUSTC_BOOTSTRAP=1\ncargo::example\ncargo::metadata=foo\ncargo::foo=bar\n",
        "\u{a0}cargo:rustc-cfg=a\n\x0b\x0ccargo::rustc-cfg=b\r\n\u{2003}\tcargo:rustc-cfg=c\n\u{85}x\n".as_bytes(),
        b"compiling things\r\nwarning: not a directive\ncargo:rustc-link-arg-bin=abc\n",
    ];
    texts.iter().map(|it| it.to_vec()).collect()
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&super::TARGET);
    }

    #[test]
    fn every_case() {
        blaze_fuzzer::every_case(&super::TARGET, super::CONTEXTS, super::check_case);
    }
}
