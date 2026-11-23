//! The parser against every documented form, the output and file limits a hostile script meets,
//! the environment of a unit, the outputs record round-tripping, the key covering every input
//! (a planted missing input caught), and the compiler's answers.

use blaze_ids::ContentId;
use blaze_own::{Alloc, String, Vec};
use blaze_sim::Config;
use blaze_unit_graph::CfgAtom;

use blaze_expand::seen::SeenRoot;

use crate::env::{DepMetadata, Given, PackageFacts, env};
use crate::instr::{Instr, Line, LinkTo, Malformed, OutputError, OutputLimits, SearchKind, Syntax, flags, lines, parse};
use crate::key::{CMode, KeyInputs, RunLimits, RunnerKey, base};
use crate::outputs::{FileError, FileLimits, FileMode, NoteKind, OutFileRef, Outputs, OutputsRef, RecordError, StoredBytes, StoredError, StoredRef, StoredWhy};
use crate::probe::{self, Table};
use crate::probe_ask::Ask;

fn instr(line: &str) -> Instr<'_> {
    match parse(line.as_bytes()) {
        Line::Instr(_, instr) => instr,
        other => panic!("{line:?}: {other:?}"),
    }
}

#[test]
fn every_documented_instruction_in_both_syntaxes() {
    for prefix in ["cargo:", "cargo::"] {
        let syntax = if prefix == "cargo::" { Syntax::New } else { Syntax::Old };
        let at = |rest: &str| format!("{prefix}{rest}");
        let cases: &[(&str, Instr<'_>)] = &[
            ("rustc-cfg=foo", Instr::Cfg { name: "foo", value: None }),
            ("rustc-cfg=feature=\"x y\"", Instr::Cfg { name: "feature", value: Some("x y") }),
            ("rustc-cfg=r#loop", Instr::Cfg { name: "r#loop", value: None }),
            ("rustc-check-cfg=cfg(foo, values(\"a\", none()))", Instr::CheckCfg("cfg(foo, values(\"a\", none()))")),
            ("rustc-env=GIT_SHA=ab=cd", Instr::Env { name: "GIT_SHA", value: "ab=cd" }),
            ("rustc-env=EMPTY=", Instr::Env { name: "EMPTY", value: "" }),
            ("rustc-link-lib=static:+whole-archive=foo:bar", Instr::LinkLib("static:+whole-archive=foo:bar")),
            ("rustc-link-search=native=/target/out", Instr::LinkSearch { kind: SearchKind::Native, path: "/target/out" }),
            ("rustc-link-search=framework=/F", Instr::LinkSearch { kind: SearchKind::Framework, path: "/F" }),
            ("rustc-link-search=dependency=/d", Instr::LinkSearch { kind: SearchKind::Dependency, path: "/d" }),
            ("rustc-link-search=crate=/c", Instr::LinkSearch { kind: SearchKind::Crate, path: "/c" }),
            ("rustc-link-search=all=/a", Instr::LinkSearch { kind: SearchKind::All, path: "/a" }),
            ("rustc-link-search=/plain", Instr::LinkSearch { kind: SearchKind::Unspecified, path: "/plain" }),
            ("rustc-link-search=C:=x", Instr::LinkSearch { kind: SearchKind::Unspecified, path: "C:=x" }),
            ("rustc-link-arg=-Wl,-z", Instr::LinkArg { to: LinkTo::All, arg: "-Wl,-z" }),
            ("rustc-link-arg-bins=-x", Instr::LinkArg { to: LinkTo::Bins, arg: "-x" }),
            ("rustc-link-arg-bin=tool=-y", Instr::LinkArg { to: LinkTo::Bin("tool"), arg: "-y" }),
            ("rustc-link-arg-tests=-t", Instr::LinkArg { to: LinkTo::Tests, arg: "-t" }),
            ("rustc-link-arg-examples=-e", Instr::LinkArg { to: LinkTo::Examples, arg: "-e" }),
            ("rustc-link-arg-benches=-b", Instr::LinkArg { to: LinkTo::Benches, arg: "-b" }),
            ("rustc-cdylib-link-arg=-c", Instr::LinkArg { to: LinkTo::Cdylib, arg: "-c" }),
            ("rustc-link-arg-cdylib=-c", Instr::LinkArg { to: LinkTo::Cdylib, arg: "-c" }),
            ("rerun-if-changed=build.rs", Instr::RerunIfChanged("build.rs")),
            ("rerun-if-env-changed=CC", Instr::RerunIfEnvChanged("CC")),
            ("warning=careful: x", Instr::Warning("careful: x")),
            ("error=no", Instr::Error("no")),
        ];
        for (rest, want) in cases {
            let line = at(rest);
            assert_eq!(parse(line.as_bytes()), Line::Instr(syntax, *want), "{line}");
        }
        // The value loses trailing whitespace (a `\r` of a Windows line), not leading.
        assert_eq!(instr(&at("warning= hi \r")), Instr::Warning(" hi"));
        assert_eq!(parse(at("rustc-flags=-l z").as_bytes()), Line::Flags(syntax, "-l z"));
        assert_eq!(parse(at("rustc-cfg").as_bytes()), Line::Malformed { key: "rustc-cfg", why: Malformed::NoValue });
        assert_eq!(parse(at("rustc-cfg=1x").as_bytes()), Line::Malformed { key: "rustc-cfg", why: Malformed::Cfg });
        assert_eq!(parse(at("rustc-cfg=a=b").as_bytes()), Line::Malformed { key: "rustc-cfg", why: Malformed::Cfg });
        assert_eq!(parse(at("rustc-cfg=a=\"b\"c\"").as_bytes()), Line::Malformed { key: "rustc-cfg", why: Malformed::Cfg });
        assert_eq!(parse(at("rustc-env=novalue").as_bytes()), Line::Malformed { key: "rustc-env", why: Malformed::Env });
        assert_eq!(parse(at("rustc-env==v").as_bytes()), Line::Malformed { key: "rustc-env", why: Malformed::Env });
        assert_eq!(parse(at("rustc-link-search=bogus=/x").as_bytes()), Line::Malformed { key: "rustc-link-search", why: Malformed::LinkSearch });
        assert_eq!(parse(at("rustc-link-search=native=").as_bytes()), Line::Malformed { key: "rustc-link-search", why: Malformed::LinkSearch });
        assert_eq!(parse(at("rustc-link-lib=").as_bytes()), Line::Malformed { key: "rustc-link-lib", why: Malformed::LinkLib });
        assert_eq!(parse(at("rustc-link-arg-bin=-y").as_bytes()), Line::Malformed { key: "rustc-link-arg-bin", why: Malformed::LinkArgBin });
    }
    // `metadata` is the new syntax's; an unknown key is metadata in the old, a warning in the new.
    assert_eq!(instr("cargo::metadata=include=/x"), Instr::Metadata { key: "include", value: "/x" });
    assert_eq!(parse(b"cargo::metadata=novalue"), Line::Malformed { key: "metadata", why: Malformed::Metadata });
    assert_eq!(instr("cargo:include=/x"), Instr::Metadata { key: "include", value: "/x" });
    assert_eq!(instr("cargo:metadata=k=v"), Instr::Metadata { key: "metadata", value: "k=v" });
    assert_eq!(parse(b"cargo::rustc-frobnicate=1"), Line::Unknown { key: "rustc-frobnicate" });
    // Not instructions.
    for plain in ["", "hello", " cargo:rustc-cfg=x", "Cargo:rustc-cfg=x", "cargo", "carg:x=y"] {
        assert_eq!(parse(plain.as_bytes()), Line::Plain, "{plain:?}");
    }
    assert_eq!(parse(b"cargo:rustc-cfg=\xff"), Line::Malformed { key: "", why: Malformed::NotUtf8 });
}

#[test]
fn rustc_flags_carry_only_l_and_big_l() {
    let got: std::vec::Vec<_> = flags("-l z -lssl -L native=/o -L/p").collect();
    assert_eq!(
        got,
        [
            Ok(Instr::LinkLib("z")),
            Ok(Instr::LinkLib("ssl")),
            Ok(Instr::LinkSearch { kind: SearchKind::Native, path: "/o" }),
            Ok(Instr::LinkSearch { kind: SearchKind::Unspecified, path: "/p" }),
        ]
    );
    assert_eq!(flags("-O").next(), Some(Err(Malformed::Flags)));
    assert_eq!(flags("-l").next(), Some(Err(Malformed::Flags)));
}

/// A hostile script's megabyte line and endless output are refused before a byte is read, the
/// line named.
#[test]
fn output_limits_stop_a_hostile_script() {
    let mut out = b"cargo:rustc-cfg=ok\ncargo:rustc-env=BIG=".to_vec();
    out.extend(std::iter::repeat_n(b'x', 1 << 20));
    out.extend_from_slice(b"\ncargo:rustc-cfg=after\n");
    let limits = OutputLimits::DEFAULT;
    match lines(&out, limits) {
        Err(OutputError::LineTooLong { line: 2, len, limit }) => assert!(len > limit && limit == 256 << 10),
        Err(other) => panic!("{other:?}"),
        Ok(_) => panic!("a megabyte line was read"),
    }
    let big = vec![b'\n'; (16 << 20) + 1];
    assert_eq!(lines(&big, limits).err(), Some(OutputError::TooLarge { len: (16 << 20) + 1, limit: 16 << 20 }));
    // The control: the same output under its line limit is read.
    let ok = OutputLimits { line: 2 << 20, output: 16 << 20 };
    assert_eq!(lines(&out, ok).map(|l| l.count()).ok(), Some(4));
}

#[test]
fn files_out_of_out_dir_or_too_large_are_refused() {
    let alloc = Alloc::global();
    let mut out = Outputs::new_in(OUT, alloc);
    let limits = FileLimits { bytes: 10, files: 2 };
    for bad in ["", "/etc/passwd", "../x.rs", "a/../../x", "a//b", "./x"] {
        assert_eq!(out.add_file(bad, b"", limits, alloc), Err(FileError::BadName), "{bad:?}");
    }
    assert_eq!(out.add_file("b.rs", b"12345", limits, alloc), Ok(()));
    assert_eq!(out.add_file("b.rs", b"1", limits, alloc), Err(FileError::Twice));
    assert_eq!(out.add_file("a/x.rs", b"123456", limits, alloc), Err(FileError::TooLarge { bytes: 11, limit: 10 }));
    assert_eq!(out.add_file("a/x.rs", b"12345", limits, alloc), Ok(()));
    assert_eq!(out.add_file("c.rs", b"", limits, alloc), Err(FileError::TooMany { limit: 2 }));
    let names: std::vec::Vec<&str> = out.files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["a/x.rs", "b.rs"]);
    assert_eq!(out.file("b.rs"), Some(&b"12345"[..]));
}

/// What a real script of each class printed, read: serde's (a version probe's cfgs) and
/// tempo-chainspec's (`rustc-env`), with unknown and malformed lines kept as notes.
#[test]
fn outputs_of_a_whole_run() {
    let alloc = Alloc::global();
    let printed = b"cargo:rerun-if-changed=build.rs\ncargo:rustc-check-cfg=cfg(no_core_net)\ncargo:rustc-cfg=no_diagnostic_namespace\nsome text\ncargo::rustc-env=TEMPO_MAINNET_NETWORK_IDENTITY={\"a\":1}\ncargo::bogus=1\ncargo:rustc-flags=-l z -O\ncargo:warning=w\n";
    let out = Outputs::read(printed, OUT, OutputLimits::DEFAULT, alloc).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(out.cfgs.len(), 1);
    assert_eq!(out.cfgs[0].0.as_str(), "no_diagnostic_namespace");
    assert_eq!(out.check_cfgs[0].as_str(), "cfg(no_core_net)");
    assert_eq!(out.env[0].1.as_str(), "{\"a\":1}");
    assert_eq!(out.link_libs[0].as_str(), "z");
    let notes: std::vec::Vec<(u32, NoteKind, &str)> = out.notes.iter().map(|n| (n.line, n.kind, n.key.as_str())).collect();
    assert_eq!(notes, [(6, NoteKind::Unknown, "bogus"), (7, NoteKind::Malformed(Malformed::Flags), "rustc-flags")]);
    assert_eq!(out.warnings.len(), 1);
}

fn round_trip(out: &Outputs<'_>) {
    let alloc = Alloc::global();
    let mut bytes = Vec::new_in(alloc);
    out.encode(&mut bytes);
    let read = OutputsRef::open(&bytes).unwrap_or_else(|e| panic!("{e:?}"));
    let owned = read.to_owned(alloc);
    assert_eq!(&owned, out);
    let mut again = Vec::new_in(alloc);
    owned.encode(&mut again);
    assert_eq!(&bytes[..], &again[..]);
    // Every proper prefix is refused, never read past its end.
    for cut in [0, 7, 8, bytes.len() / 2, bytes.len() - 1] {
        if cut < bytes.len() {
            assert!(OutputsRef::open(&bytes[..cut]).is_err(), "prefix {cut}");
        }
    }
}

/// Any output a script prints, and any files, round-trip byte for byte.
#[test]
fn the_record_round_trips_byte_for_byte() {
    const KEYS: &[&str] = &[
        "rustc-cfg=a",
        "rustc-cfg=k=\"v\"",
        "rustc-check-cfg=cfg(a)",
        "rustc-env=N=v",
        "rustc-link-lib=z",
        "rustc-link-search=native=/o",
        "rustc-link-arg-bin=b=-x",
        "rustc-link-arg=-y",
        "metadata=k=v",
        "include=/x",
        "warning=w",
        "error=e",
        "unknown=1",
        "rustc-cfg",
        "rustc-flags=-l q -Z",
        "rerun-if-changed=x",
    ];
    blaze_sim::check(Config::DEFAULT, |tape| {
        let alloc = Alloc::global();
        let mut printed = std::vec::Vec::new();
        while tape.more(4, "line") {
            match tape.coin("instruction") {
                true => {
                    printed.extend_from_slice(if tape.coin("new") { b"cargo::" } else { b"cargo:" });
                    printed.extend_from_slice(tape.pick(KEYS, "key").copied().unwrap_or("").as_bytes());
                }
                false => {
                    let mut noise = [0u8; 6];
                    tape.fill(&mut noise, "noise");
                    printed.extend(noise.iter().filter(|b| **b != b'\n'));
                }
            }
            printed.push(b'\n');
        }
        let out_dir = *tape.pick(&[OUT, "/target/debug/build/y-fedcba9876543210/out"], "out dir").unwrap_or(&OUT);
        let mut out = Outputs::read(&printed, out_dir, OutputLimits::DEFAULT, alloc).unwrap_or_else(|e| panic!("{e:?}"));
        while tape.more(3, "file") {
            let name = format!("f{}/x.rs", tape.below(5, "name"));
            let mut bytes = [0u8; 9];
            tape.fill(&mut bytes, "bytes");
            let _ = out.add_file(&name, &bytes[..tape.index(10, "len")], FileLimits::DEFAULT, alloc);
        }
        while tape.more(3, "stored file") {
            let name = format!("f{}/lib.a", tape.below(5, "name"));
            let mut sha256 = [0u8; 32];
            tape.fill(&mut sha256, "digest");
            let mode = if tape.coin("executable") { FileMode::Executable } else { FileMode::Plain };
            let len = tape.below(1 << 20, "len") << 12;
            let _ = out.add_stored(&name, len, mode, sha256, FileLimits::DEFAULT, alloc);
        }
        round_trip(&out);
    });
}

#[test]
fn a_record_with_a_bad_kind_or_layout_is_refused() {
    let alloc = Alloc::global();
    let mut out = Outputs::new_in(OUT, alloc);
    out.link_search.push((SearchKind::Native, String::from_str_in("/o", alloc)));
    let mut bytes = Vec::new_in(alloc);
    out.encode(&mut bytes);
    // The kind byte follows the layout word, the `OUT_DIR` and four counts, then this
    // section's count.
    let at = 8 + 4 + OUT.len() + 4 * 4 + 4;
    assert_eq!(bytes[at], SearchKind::Native as u8);
    bytes[at] = 9;
    assert_eq!(OutputsRef::open(&bytes).err(), Some(RecordError::Kind { at }));
    bytes[0] ^= 1;
    assert_eq!(OutputsRef::open(&bytes).err(), Some(RecordError::Layout));
    // An `OUT_DIR` that is not absolute would let any path name a file of the record.
    let mut relative = Vec::new_in(alloc);
    Outputs::new_in("out", alloc).encode(&mut relative);
    assert_eq!(OutputsRef::open(&relative).err(), Some(RecordError::OutDir { at: 8 }));
}

/// The `OUT_DIR` of the tests' records: the shape `env` gives a unit.
const OUT: &str = "/target/debug/build/x-0123456789abcdef/out";

/// A host that answers nothing: every ask the record does not answer reaches it.
struct Nothing;

impl blaze_expand::ExpandCx for Nothing {
    fn env(&self, _: &str) -> Result<Option<&str>, blaze_expand::HostError> {
        Ok(None)
    }

    fn cfg(&self, _: &blaze_expand::TokenStream<'_>) -> Result<bool, blaze_expand::HostError> {
        Err(blaze_expand::HostError { ask: blaze_expand::HostAsk::Cfg, why: blaze_expand::HostWhy::Refused })
    }

    fn include(&self, _: &str, _: blaze_expand::IncludeAs) -> Result<blaze_expand::Included<'_>, blaze_expand::HostError> {
        Ok(blaze_expand::Included::Missing)
    }

    fn place(&self, _: blaze_expand::PlaceOf) -> Result<blaze_expand::Place<'_>, blaze_expand::HostError> {
        Ok(blaze_expand::Place::Number(1))
    }

    fn run_proc_macro<'a>(
        &self,
        _: &blaze_expand::MacroDefinition<'_>,
        _: blaze_expand::ProcCall<'_>,
        _: Alloc<'a>,
    ) -> Result<blaze_expand::Produced<'a>, blaze_expand::HostError> {
        Err(blaze_expand::HostError { ask: blaze_expand::HostAsk::ProcMacro, why: blaze_expand::HostWhy::Refused })
    }
}

/// mime_guess 2.0.5 prints `rustc-env=MIME_TYPES_GENERATED_PATH=<OUT_DIR>/mime_types_generated.rs`
/// and its library reads `include!(env!("MIME_TYPES_GENERATED_PATH"))`: the path the script saw
/// must name the record's file, and `env!("OUT_DIR")` must be that same folder. (Before the
/// record kept its `OUT_DIR`, the crate was told `/out` and this include fell through.)
#[test]
fn a_path_the_script_printed_into_out_dir_names_the_records_file() {
    use blaze_expand::{ExpandCx, IncludeAs, Included};
    let alloc = Alloc::global();
    let printed = format!("cargo:rustc-env=MIME_TYPES_GENERATED_PATH={OUT}/mime_types_generated.rs\n");
    let mut out = Outputs::read(printed.as_bytes(), OUT, OutputLimits::DEFAULT, alloc).unwrap_or_else(|e| panic!("{e:?}"));
    out.add_file("mime_types_generated.rs", b"static X: u8 = 1;", FileLimits::DEFAULT, alloc).unwrap_or_else(|e| panic!("{e:?}"));
    let mut bytes = Vec::new_in(alloc);
    out.encode(&mut bytes);
    let cx = crate::cx::ScriptCx { inner: Nothing, out: OutputsRef::open(&bytes).unwrap_or_else(|e| panic!("{e:?}")) };
    assert_eq!(cx.env("OUT_DIR").ok(), Some(Some(OUT)));
    let path = cx.env("MIME_TYPES_GENERATED_PATH").ok().flatten().unwrap_or_else(|| panic!("no MIME_TYPES_GENERATED_PATH"));
    assert!(matches!(cx.include(path, IncludeAs::Str), Ok(Included::Bytes(b"static X: u8 = 1;"))));
    let joined = format!("{}/mime_types_generated.rs", cx.env("OUT_DIR").ok().flatten().unwrap_or(""));
    assert!(matches!(cx.include(&joined, IncludeAs::Str), Ok(Included::Bytes(b"static X: u8 = 1;"))));
    // A file the record lacks under `OUT_DIR` is missing, not the host's; a path outside it is
    // the host's.
    assert!(matches!(cx.include(&format!("{OUT}/other.rs"), IncludeAs::Str), Ok(Included::Missing)));
    assert!(matches!(cx.include("/out/mime_types_generated.rs", IncludeAs::Str), Ok(Included::Missing)));
}

/// A test's file area: one file, by its digest.
struct OneFile {
    sha256: [u8; 32],
    bytes: &'static [u8],
}

impl StoredBytes for OneFile {
    fn bytes(&self, file: &StoredRef<'_>) -> Result<&[u8], StoredError> {
        if *file.sha256 == self.sha256 { Ok(self.bytes) } else { Err(StoredError { sha256: *file.sha256, why: StoredWhy::NotThere }) }
    }
}

/// A stored file (a native script's `OUT_DIR` file, its bytes in the home's file area) is
/// named by the record and read through the area only when asked: `include!` of it is its
/// bytes; with no area it is an unreadable include, never missing and never another's bytes; a
/// file of the wrong length is refused; one name is never in both sections; the crate's
/// digest covers the stored entry.
/// Expected: `test tests::a_stored_file_is_read_through_the_area_only_when_asked ... ok`.
#[test]
fn a_stored_file_is_read_through_the_area_only_when_asked() {
    use blaze_expand::{ExpandCx, HostWhy, IncludeAs, Included};
    let alloc = Alloc::global();
    let text: &'static [u8] = b"pub const BINDINGS: u8 = 7;";
    let sha256 = [7u8; 32];
    let mut out = Outputs::new_in(OUT, alloc);
    out.add_stored("bindings.rs", text.len() as u64, FileMode::Plain, sha256, FileLimits::DEFAULT, alloc).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(out.add_file("bindings.rs", b"", FileLimits::DEFAULT, alloc), Err(FileError::Twice));
    assert_eq!(out.add_stored("bindings.rs", 1, FileMode::Plain, sha256, FileLimits::DEFAULT, alloc), Err(FileError::Twice));
    // Only the count of files is limited: a stored file of any length is taken.
    out.add_stored("lib/librocksdb.a", 810_375_632 << 4, FileMode::Plain, [9u8; 32], FileLimits { bytes: 10, files: 3 }, alloc).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(out.add_stored("x.a", 1, FileMode::Plain, [1u8; 32], FileLimits { bytes: 10, files: 2 }, alloc), Err(FileError::TooMany { limit: 2 }));
    round_trip(&out);
    let mut bytes = Vec::new_in(alloc);
    out.encode(&mut bytes);
    let read = OutputsRef::open(&bytes).unwrap_or_else(|e| panic!("{e:?}"));
    let bindings = read.file("bindings.rs").unwrap_or_else(|| panic!("the record names bindings.rs"));
    assert!(matches!(bindings, OutFileRef::Stored(StoredRef { len: 27, mode: FileMode::Plain, .. })), "{bindings:?}");
    assert_eq!(read.bytes_of(bindings).err().map(|e| e.why), Some(StoredWhy::NoArea));
    let path = format!("{OUT}/bindings.rs");
    let bare = crate::cx::ScriptCx { inner: Nothing, out: read };
    assert!(matches!(bare.include(&path, IncludeAs::Str), Err(blaze_expand::HostError { why: HostWhy::Unreadable, .. })));
    let area = OneFile { sha256, bytes: text };
    let cx = crate::cx::ScriptCx { inner: Nothing, out: read.with_area(&area) };
    assert!(matches!(cx.include(&path, IncludeAs::Str), Ok(Included::Bytes(b"pub const BINDINGS: u8 = 7;"))));
    // The area's file of another length than the record's is refused.
    let short = OneFile { sha256, bytes: b"pub" };
    assert_eq!(read.with_area(&short).bytes_of(bindings).err().map(|e| e.why), Some(StoredWhy::Length { found: 3 }));
    // A file the area lacks is named by its digest.
    let archive = read.file("lib/librocksdb.a").unwrap_or_else(|| panic!("the record names the archive"));
    assert_eq!(read.with_area(&area).bytes_of(archive).err().map(|e| e.why), Some(StoredWhy::NotThere));
    // The crate's digest covers the stored entry (control: one byte of its digest moved).
    let mut other = Outputs::new_in(OUT, alloc);
    other.add_stored("bindings.rs", text.len() as u64, FileMode::Plain, [8u8; 32], FileLimits::DEFAULT, alloc).unwrap_or_else(|e| panic!("{e:?}"));
    let mut same = Outputs::new_in(OUT, alloc);
    same.add_stored("bindings.rs", text.len() as u64, FileMode::Plain, sha256, FileLimits::DEFAULT, alloc).unwrap_or_else(|e| panic!("{e:?}"));
    let digest = |o: &Outputs<'_>| {
        let mut b = Vec::new_in(alloc);
        o.encode(&mut b);
        OutputsRef::open(&b).map(|r| r.crate_digest()).unwrap_or_else(|e| panic!("{e:?}"))
    };
    assert_ne!(digest(&same), digest(&other));
    assert_eq!(digest(&same), digest(&same));
}

fn given_env<'a>(features: &'a [blaze_unit_graph::Text<'a>], cfg: &'a [CfgAtom<'a>], deps: &'a [DepMetadata<'a>]) -> Given<'a, 'a> {
    Given {
        package: PackageFacts {
            name: "zstd-sys",
            version: "2.1.2+zstd.1.5.7",
            meta: Some(blaze_unit_graph::PackageMetaVars { authors: "a:b", ..blaze_unit_graph::PackageMetaVars::default() }),
            links: Some("zstd"),
        },
        seen: SeenRoot::Package,
        features,
        target_cfg: cfg,
        target: "aarch64-apple-darwin",
        host: "aarch64-apple-darwin",
        deps,
    }
}

#[test]
fn the_environment_of_a_unit() {
    let alloc = Alloc::global();
    let features = [String::from_str_in("legacy", alloc), String::from_str_in("zdict-builder", alloc)];
    let mut cfg = [
        CfgAtom::pair("target_feature", "aes", alloc),
        CfgAtom::pair("target_feature", "neon", alloc),
        CfgAtom::pair("target_os", "macos", alloc),
        CfgAtom::flag("unix", alloc),
        CfgAtom::flag("debug_assertions", alloc),
        CfgAtom::flag("test", alloc),
        CfgAtom::pair("feature", "legacy", alloc),
    ];
    cfg.sort();
    let deps = [DepMetadata { links: "z-lib", key: "include", value: "/workspace/x" }];
    let given = given_env(&features, &cfg, &deps);
    let env = env(&given, alloc);
    let get = |name: &str| env.get(name).unwrap_or_else(|| panic!("{name} unset"));
    // D96 (2): a dependency's script sees the one package root, never `/workspace`.
    assert_eq!(get("CARGO_MANIFEST_DIR"), "/package");
    assert_eq!(get("CARGO_MANIFEST_PATH"), "/package/Cargo.toml");
    // D96 (3): a member's sees its dir under its workspace.
    let member = Given { seen: SeenRoot::member("crates/zstd").expect("a plain dir"), ..given };
    assert_eq!(crate::env::env(&member, alloc).get("CARGO_MANIFEST_DIR"), Some("/workspace/crates/zstd"));
    assert_eq!(get("CARGO_MANIFEST_LINKS"), "zstd");
    assert_eq!(get("CARGO_FEATURE_ZDICT_BUILDER"), "1");
    assert_eq!(get("CARGO_CFG_TARGET_FEATURE"), "aes,neon");
    assert_eq!(get("CARGO_CFG_UNIX"), "");
    assert_eq!(get("CARGO_CFG_DEBUG_ASSERTIONS"), "");
    assert_eq!(get("CARGO_CFG_FEATURE"), "legacy");
    assert_eq!(env.get("CARGO_CFG_TEST"), None);
    assert_eq!(get("CARGO_PKG_VERSION_MAJOR"), "2");
    assert_eq!(get("CARGO_PKG_VERSION_PATCH"), "2");
    assert_eq!(get("CARGO_PKG_VERSION_PRE"), "");
    assert_eq!(get("CARGO_PKG_AUTHORS"), "a:b");
    assert_eq!(get("CARGO_PKG_DESCRIPTION"), "");
    assert_eq!(get("DEP_Z_LIB_INCLUDE"), "/workspace/x");
    assert_eq!(get("RUSTC"), "rustc");
    assert_eq!(env.get("RUSTC_WRAPPER"), None);
    assert_eq!(env.get("PATH"), None);
    // `cc` reads the deployment target before it would ask `xcrun`; control: no other target
    // is given one.
    assert_eq!(get("MACOSX_DEPLOYMENT_TARGET"), crate::toolchain::MACOS_MIN);
    let linux = Given { target: "aarch64-unknown-linux-gnu", ..given };
    assert_eq!(crate::env::env(&linux, alloc).get("MACOSX_DEPLOYMENT_TARGET"), None);
    // tempo-node reads the profile as the fourth part from the end of `OUT_DIR`.
    let out_dir = get("OUT_DIR");
    assert_eq!(out_dir.rsplit('/').nth(3), Some("debug"));
    assert!(out_dir.starts_with("/target/debug/build/zstd-sys-") && out_dir.ends_with("/out"), "{out_dir}");
    // Sorted, each once; and a function of its inputs alone.
    assert!(env.vars.windows(2).all(|w| w[0].0.as_str() < w[1].0.as_str()));
    assert_eq!(env, crate::env::env(&given, alloc));
    // Another feature set is another `OUT_DIR`.
    let fewer = [String::from_str_in("legacy", alloc)];
    let other = crate::env::env(&given_env(&fewer, &cfg, &deps), alloc);
    assert_ne!(other.get("OUT_DIR"), Some(out_dir));
}

/// The job variables are the run's, never the key's: the keyed environment has no `NUM_JOBS`;
/// a wasm run is told `NUM_JOBS=1` and no makeflags; a native run the worker count and
/// `CARGO_MAKEFLAGS` in cargo's exact text. Two job counts give two run environments and ONE key
/// (the key takes `Env`, which `for_run` leaves as it was). Control: a key over the run
/// environment's variables (the old `NUM_JOBS` in `env`) would differ by the count.
#[test]
fn script_jobs_are_told_to_the_run_and_never_keyed() {
    use crate::env::{Env, Jobs, job_vars, makeflags};
    let alloc = Alloc::global();
    let cfg = [CfgAtom::flag("unix", alloc)];
    let keyed = env(&given_env(&[], &cfg, &[]), alloc);
    assert_eq!(keyed.get("NUM_JOBS"), None);
    assert_eq!(keyed.get("CARGO_MAKEFLAGS"), None);

    let wasm = keyed.for_run(Jobs::One, alloc);
    assert_eq!(wasm.get("NUM_JOBS"), Some("1"));
    assert_eq!(wasm.get("CARGO_MAKEFLAGS"), None);
    assert_eq!(wasm.vars.len(), keyed.vars.len() + 1);

    let fifo = "/h/.blaze/v2/native-work/blaze/jobs/4242";
    assert_eq!(makeflags(fifo, alloc).as_str(), "-j --jobserver-fds=fifo:/h/.blaze/v2/native-work/blaze/jobs/4242 --jobserver-auth=fifo:/h/.blaze/v2/native-work/blaze/jobs/4242");
    let shared = |n: u32| Jobs::Shared { workers: core::num::NonZeroU32::new(n).expect("a count"), fifo };
    let native = keyed.for_run(shared(12), alloc);
    assert_eq!(native.get("NUM_JOBS"), Some("12"));
    assert_eq!(native.get("CARGO_MAKEFLAGS"), Some(makeflags(fifo, alloc).as_str()));
    assert_eq!(native.vars.len(), keyed.vars.len() + 2);
    assert!(native.vars.windows(2).all(|w| w[0].0.as_str() < w[1].0.as_str()), "sorted, each once");
    // Every keyed variable reaches the run as it was.
    for (name, value) in keyed.vars.iter() {
        assert_eq!(native.get(name), Some(value.as_str()));
    }
    let names: std::vec::Vec<&str> = job_vars(shared(128), alloc).iter().map(|(n, _)| *n).collect();
    assert_eq!(names, ["CARGO_MAKEFLAGS", "NUM_JOBS"]);

    // One key for two job counts: the key is made of `keyed`, which no count touches.
    let key = |e: &Env<'_>| base(&KeyInputs { module: [1; 32], env: e, answers: Table::EMPTY.id(), c_mode: CMode::Deferred, limits: LIMITS, runner: RunnerKey::Wasm }, &mut Vec::new_in(alloc));
    let (two, many) = (keyed.for_run(shared(2), alloc), keyed.for_run(shared(64), alloc));
    assert_ne!(two, many, "the runs are told different counts");
    assert_eq!(key(&keyed), key(&keyed.clone()));
    // Control: had the count been keyed (an `Env` holding the run's variables), the keys differ.
    fn as_keyed<'a>(run: &crate::env::RunEnv<'a>) -> Env<'a> {
        Env { vars: run.vars.clone() }
    }
    assert_ne!(key(&as_keyed(&two)), key(&as_keyed(&many)));
    println!("jobs: wasm NUM_JOBS=1; native NUM_JOBS=12 and CARGO_MAKEFLAGS={}; one key for 2 and 64", makeflags(fifo, alloc).as_str());
}

const LIMITS: RunLimits = RunLimits { fuel: 1 << 30, memory: 1 << 28, out_dir_bytes: 64 << 20, out_dir_files: 4096, output: 16 << 20, line: 256 << 10 };

/// Which input of the key `key` misses, if any: each input changed alone must change the key.
fn missed(key: impl Fn(&KeyInputs<'_>) -> ContentId) -> Option<&'static str> {
    let alloc = Alloc::global();
    let features = [String::from_str_in("std", alloc)];
    let cfg = [CfgAtom::flag("unix", alloc)];
    let env_a = env(&given_env(&features, &cfg, &[]), alloc);
    let env_b = env(&given_env(&[], &cfg, &[]), alloc);
    let at = KeyInputs {
        module: [1; 32],
        env: &env_a,
        answers: Table::EMPTY.id(),
        c_mode: CMode::Deferred,
        limits: LIMITS,
        runner: RunnerKey::Wasm,
    };
    const SDK: &str = "macos-sdk 27.2";
    let native = KeyInputs { runner: RunnerKey::Native { compiler: [4; 32], target: "aarch64-apple-darwin", sysroot: SDK, host_tools: &[] }, ..at };
    // D71: a granted perl, and the same perl with another binary.
    let perl = [crate::key::HostToolKey { name: "perl", digest: [8; 32], runs_out_dir: false }];
    let other_perl = [crate::key::HostToolKey { name: "perl", digest: [9; 32], runs_out_dir: false }];
    // D112, rule 21: the same perl whose row also grants starting what is under OUT_DIR.
    let perl_out_dir = [crate::key::HostToolKey { name: "perl", digest: [8; 32], runs_out_dir: true }];
    let with_perl = KeyInputs { runner: RunnerKey::Native { compiler: [4; 32], target: "aarch64-apple-darwin", sysroot: SDK, host_tools: &perl }, ..at };
    let row = [probe::Row { key: ContentId([7; 32]), accepted: true }];
    let changed: [(&str, KeyInputs<'_>, KeyInputs<'_>); 12] = [
        ("module", at, KeyInputs { module: [2; 32], ..at }),
        ("env", at, KeyInputs { env: &env_b, ..at }),
        ("answers", at, KeyInputs { answers: Table { rows: &row }.id(), ..at }),
        ("c_mode", at, KeyInputs { c_mode: CMode::Built, ..at }),
        ("limits", at, KeyInputs { limits: RunLimits { fuel: 1, ..LIMITS }, ..at }),
        ("runner", at, native),
        ("compiler", native, KeyInputs { runner: RunnerKey::Native { compiler: [5; 32], target: "aarch64-apple-darwin", sysroot: SDK, host_tools: &[] }, ..at }),
        ("target", native, KeyInputs { runner: RunnerKey::Native { compiler: [4; 32], target: "aarch64-unknown-linux-gnu", sysroot: SDK, host_tools: &[] }, ..at }),
        ("sysroot", native, KeyInputs { runner: RunnerKey::Native { compiler: [4; 32], target: "aarch64-apple-darwin", sysroot: "macos-sdk 27.3", host_tools: &[] }, ..at }),
        ("host tool", native, with_perl),
        (
            "host tool digest",
            with_perl,
            KeyInputs { runner: RunnerKey::Native { compiler: [4; 32], target: "aarch64-apple-darwin", sysroot: SDK, host_tools: &other_perl }, ..at },
        ),
        (
            "host tool runs_out_dir",
            with_perl,
            KeyInputs { runner: RunnerKey::Native { compiler: [4; 32], target: "aarch64-apple-darwin", sysroot: SDK, host_tools: &perl_out_dir }, ..at },
        ),
    ];
    changed.iter().find(|(_, from, to)| key(from) == key(to)).map(|(name, _, _)| *name)
}

/// D112, rule 21: two host-tool rows that differ only in `runs_out_dir` (the grant to start
/// what the script wrote under OUT_DIR, which widens the sandbox) give two run keys. The rows
/// are parsed from text by `host_tools` and keyed by `HostToolKey::of`, as native_unit keys
/// them; a key that drops the field (in `base` or in `of`) gives one key and fails here.
/// Prints "runs_out_dir keyed: two rows, two keys".
#[test]
fn a_row_changed_only_in_runs_out_dir_changes_the_key() {
    let alloc = blaze_own::testing::heap().alloc();
    let yes = r#"{"host_tools": {"t": {"sh": {"path": "/bin/sh", "reads": "", "runs": "", "needed_by": "x", "runs_out_dir": "yes"}, "names": "sh"}}}"#;
    let no = r#"{"host_tools": {"t": {"sh": {"path": "/bin/sh", "reads": "", "runs": "", "needed_by": "x", "runs_out_dir": "no"}, "names": "sh"}}}"#;
    let features = [String::from_str_in("std", alloc)];
    let e = env(&given_env(&features, &[], &[]), alloc);
    let key_of = |text: &str| -> ContentId {
        let tools = crate::host_tools::host_tools(text, "t", alloc).expect("a whole row");
        let keys = [crate::key::HostToolKey::of(&tools[0], "sh", [8; 32])];
        let inputs = KeyInputs {
            module: [1; 32],
            env: &e,
            answers: Table::EMPTY.id(),
            c_mode: CMode::Built,
            limits: LIMITS,
            runner: RunnerKey::Native { compiler: [4; 32], target: "aarch64-apple-darwin", sysroot: "macos-sdk 27.2", host_tools: &keys },
        };
        base(&inputs, &mut Vec::new_in(alloc))
    };
    assert_eq!(key_of(yes), key_of(yes), "one row, one key");
    assert_ne!(key_of(yes), key_of(no), "rows differing only in runs_out_dir share a key (rule 21)");
    println!("runs_out_dir keyed: two rows, two keys");
}

#[test]
fn the_key_covers_every_input() {
    let alloc = Alloc::global();
    let mut scratch = Vec::new_in(alloc);
    assert_eq!(missed(|inputs| base(inputs, &mut Vec::new_in(Alloc::global()))), None);
    // A planted key that forgets the environment is caught by the same check.
    let planted = |inputs: &KeyInputs<'_>| {
        let empty = crate::env::Env { vars: Vec::new_in(Alloc::global()) };
        base(&KeyInputs { env: &empty, ..*inputs }, &mut Vec::new_in(Alloc::global()))
    };
    assert_eq!(missed(planted), Some("env"));
    // Equal inputs, equal keys, whatever the scratch held.
    scratch.extend_from_slice(b"junk");
    let features = [String::from_str_in("std", alloc)];
    let e = env(&given_env(&features, &[], &[]), alloc);
    let inputs = KeyInputs {
        module: [3; 32],
        env: &e,
        answers: Table::EMPTY.id(),
        c_mode: CMode::Deferred,
        limits: LIMITS,
        runner: RunnerKey::Wasm,
    };
    assert_eq!(base(&inputs, &mut scratch), base(&inputs, &mut Vec::new_in(alloc)));
}

#[test]
fn the_compiler_answers_its_version_and_refuses_the_rest() {
    let alloc = Alloc::global();
    let ask = |args: &[&str]| -> Result<std::string::String, crate::probe_ask::Refused> {
        Ok(match crate::probe_ask::classify(args, alloc)? {
            Ask::Version { verbose } => std::format!("version verbose={verbose}"),
            Ask::Print { what, .. } => std::format!("print {what}"),
            Ask::Compile(_) => std::format!("compile"),
        })
    };
    assert_eq!(ask(&["--version"]).as_deref(), Ok("version verbose=false"));
    assert_eq!(ask(&["-vV"]).as_deref(), Ok("version verbose=true"));
    assert_eq!(ask(&["-Vv"]).as_deref(), Ok("version verbose=true"));
    assert_eq!(ask(&["--version", "--verbose"]).as_deref(), Ok("version verbose=true"));
    assert_eq!(ask(&["--verbose", "--version"]).as_deref(), Ok("version verbose=true"));
    assert_eq!(ask(&["--crate-type=lib", "--emit=metadata", "-"]).as_deref(), Ok("compile"));
    assert_eq!(probe::version(false, "aarch64-apple-darwin", alloc).as_str(), format!("{}\n", blaze_pins::RUSTC_V));
    let verbose = probe::version(true, "aarch64-apple-darwin", alloc);
    let tail = format!("\nhost: aarch64-apple-darwin\nrelease: {}\nLLVM version: {}\n", blaze_pins::RELEASE, blaze_pins::RUSTC_LLVM);
    assert!(verbose.as_str().contains(&tail), "{}", verbose.as_str());
    // A try-compile is keyed without its path, by its snippet; the empty table answers none.
    let key = |args: &[&str], snippet: &[u8]| match crate::probe_ask::classify(args, alloc) {
        Ok(Ask::Compile(compile)) => compile.key("aarch64-apple-darwin", snippet),
        other => panic!("not a compile: {other:?}"),
    };
    let a = key(&["--emit=metadata", "/target/debug/build/x-1/out/probe.rs"], b"fn f() {}");
    let b = key(&["--emit=metadata", "/target/debug/build/x-2/out/probe.rs"], b"fn f() {}");
    let c = key(&["--emit=metadata", "/p.rs"], b"fn g() {}");
    assert_eq!(a, b);
    assert_ne!(a, c);
    assert_eq!(Table::EMPTY.answer(&a), None);
    let rows = [probe::Row { key: a, accepted: true }];
    assert_eq!(Table { rows: &rows }.answer(&a), Some(true));
}

