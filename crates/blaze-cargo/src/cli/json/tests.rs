use blaze_api::{
    Artifact, ArtifactKind, BuildScriptRan, Diagnostic, Level, Span, TargetKind, UnitOf, UnitProfile, UnitTarget,
};
use blaze_ids::ContentId;

use super::*;

/// `it` as the daemon sends it, for [`lent`] to read in place as the CLI does.
fn sent<T: serde::Serialize>(it: &T) -> Vec<u8> {
    blaze_protocol::cbor::to_vec(it).unwrap()
}

/// A `*Ref` read where it lies in [`sent`]'s bytes.
fn lent<'a, T: serde::Deserialize<'a>>(bytes: &'a [u8]) -> T {
    blaze_protocol::cbor::from_slice(bytes).unwrap()
}

fn bin_target(dir: &str) -> UnitTarget {
    UnitTarget {
        kind: TargetKind::Bin,
        crate_types: vec![CrateType::Bin],
        name: "foo".to_owned(),
        src_path: format!("{dir}/src/main.rs"),
        edition: "2021".to_owned(),
        doc: true,
        doctest: false,
        test: true,
    }
}

const TARGET: &str = r#"{"kind":["bin"],"crate_types":["bin"],"name":"foo","src_path":"/w/foo/src/main.rs","edition":"2021","doc":true,"doctest":false,"test":true}"#;

#[test]
fn build_finished() {
    assert_eq!(line(|out| super::build_finished(out, true)), r#"{"reason":"build-finished","success":true}"#);
    assert_eq!(line(|out| super::build_finished(out, false)), r#"{"reason":"build-finished","success":false}"#);
}

#[test]
fn compiler_artifact() {
    let artifact = Artifact {
        package: "path+file:///w/foo#0.1.0".to_owned(),
        kind: ArtifactKind::Bin,
        path: "/w/foo/target/debug/foo".to_owned(),
        content: ContentId([0; 32]),
        manifest_path: "/w/foo/Cargo.toml".to_owned(),
        target: bin_target("/w/foo"),
        profile: UnitProfile {
            opt_level: "0".to_owned(),
            debuginfo: 2,
            debug_assertions: true,
            overflow_checks: true,
            test: false,
        },
        features: vec!["default".to_owned()],
        filenames: vec!["/w/foo/target/debug/foo".to_owned()],
        executable: Some("/w/foo/target/debug/foo".to_owned()),
        fresh: false,
        runner: Vec::new(),
        env: vec![("CARGO_PKG_NAME".to_owned(), "foo".to_owned())],
        harness: true,
    };
    let expected = format!(
        r#"{{"reason":"compiler-artifact","package_id":"path+file:///w/foo#0.1.0","manifest_path":"/w/foo/Cargo.toml","target":{TARGET},"profile":{{"opt_level":"0","debuginfo":2,"debug_assertions":true,"overflow_checks":true,"test":false}},"features":["default"],"filenames":["/w/foo/target/debug/foo"],"executable":"/w/foo/target/debug/foo","fresh":false}}"#
    );
    assert_eq!(line(|out| super::artifact(out, &artifact)), expected);
    let lib = Artifact { executable: None, fresh: true, ..artifact };
    assert!(line(|out| super::artifact(out, &lib)).ends_with(r#""executable":null,"fresh":true}"#));
}

#[test]
fn build_script_executed() {
    let ran = BuildScriptRan {
        package_id: "path+file:///w/foo#0.1.0".to_owned(),
        linked_libs: vec!["static=z".to_owned()],
        linked_paths: vec!["native=/w/foo/target/debug/build/foo-1/out".to_owned()],
        cfgs: vec!["has_z".to_owned()],
        env: vec![("Z_VERSION".to_owned(), "1.3".to_owned())],
        out_dir: "/w/foo/target/debug/build/foo-1/out".to_owned(),
    };
    let bytes = sent(&ran);
    assert_eq!(
        line(|out| build_script(out, lent(&bytes))),
        r#"{"reason":"build-script-executed","package_id":"path+file:///w/foo#0.1.0","linked_libs":["static=z"],"linked_paths":["native=/w/foo/target/debug/build/foo-1/out"],"cfgs":["has_z"],"env":[["Z_VERSION","1.3"]],"out_dir":"/w/foo/target/debug/build/foo-1/out"}"#
    );
}

#[test]
fn strings_as_serde_json_writes_them() {
    let text = "a\"b\\c\nd\te\u{1b}[0m\u{7f}é/";
    assert_eq!(line(|out| quote(out, text)), serde_json::to_string(text).unwrap());
}

fn span(file: &str, (line, column): (u32, u32), (end_line, end_column): (u32, u32), bytes: (u32, u32)) -> Span {
    Span {
        file: file.to_owned(),
        line,
        column,
        end_line,
        end_column,
        label: None,
        byte_start: bytes.0,
        byte_end: bytes.1,
        primary: true,
        replacement: None,
        applicability: None,
    }
}

fn unused(rendered: &str) -> Diagnostic {
    let at = span("src/main.rs", (2, 9), (2, 10), (20, 21));
    let help = Diagnostic {
        level: Level::Help,
        code: None,
        message: "if this is intentional, prefix it with an underscore".to_owned(),
        spans: vec![Span {
            replacement: Some("_x".to_owned()),
            applicability: Some(Applicability::MachineApplicable),
            ..at.clone()
        }],
        children: Vec::new(),
        facts: Vec::new(),
        rendered: String::new(),
        unit: None,
        explanation: None,
    };
    Diagnostic {
        level: Level::Warning,
        code: Some("unused_variables".to_owned()),
        message: "unused variable: `x`".to_owned(),
        spans: vec![at],
        children: vec![help],
        facts: Vec::new(),
        rendered: rendered.to_owned(),
        unit: None,
        explanation: None,
    }
}

fn checkout() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {\r\n    let x = 1;\r\n}\r\n").unwrap();
    std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
    let root = dir.path().display().to_string();
    (dir, root)
}

/// The `compiler-message` line of `diagnostic` in `unit`, both read in place as the CLI reads them.
fn message(unit: &UnitOf, diagnostic: &Diagnostic, ansi: bool, sources: &mut Sources) -> String {
    let (unit, diagnostic) = (sent(unit), sent(diagnostic));
    line(|out| compiler_message(out, lent(&unit), lent(&diagnostic), ansi, sources))
}

#[test]
fn a_compiler_message_as_cargo_writes_it_again() {
    let (_dir, root) = checkout();
    let unit = UnitOf {
        id: 0,
        package_id: format!("path+file://{root}#0.1.0"),
        manifest_path: format!("{root}/Cargo.toml"),
        target: bin_target(&root),
        test: false,
    };
    let rendered = "\u{1b}[1m\u{1b}[33mwarning\u{1b}[0m: unused variable: `x`\n\n";
    let mut sources = Sources::new(std::path::Path::new(&root));
    let said = message(&unit, &unused(rendered), false, &mut sources);
    let target = format!(
        r#"{{"kind":["bin"],"crate_types":["bin"],"name":"foo","src_path":"{root}/src/main.rs","edition":"2021","doc":true,"doctest":false,"test":true}}"#
    );
    let text = r#"[{"highlight_end":10,"highlight_start":9,"text":"    let x = 1;"}]"#;
    let expected = format!(
        concat!(
            r#"{{"reason":"compiler-message","package_id":"path+file://{root}#0.1.0","manifest_path":"{root}/Cargo.toml","target":{target},"#,
            r#""message":{{"rendered":"warning: unused variable: `x`\n\n","$message_type":"diagnostic","#,
            r#""children":[{{"children":[],"code":null,"level":"help","message":"if this is intentional, prefix it with an underscore","rendered":null,"#,
            r#""spans":[{{"byte_end":21,"byte_start":20,"column_end":10,"column_start":9,"expansion":null,"file_name":"src/main.rs","is_primary":true,"label":null,"line_end":2,"line_start":2,"suggested_replacement":"_x","suggestion_applicability":"MachineApplicable","text":{text}}}]}}],"#,
            r#""level":"warning","message":"unused variable: `x`","#,
            r#""spans":[{{"byte_end":21,"byte_start":20,"column_end":10,"column_start":9,"expansion":null,"file_name":"src/main.rs","is_primary":true,"label":null,"line_end":2,"line_start":2,"suggested_replacement":null,"suggestion_applicability":null,"text":{text}}}],"#,
            r#""code":{{"code":"unused_variables","explanation":null}}}}}}"#,
        ),
        root = root,
        target = target,
        text = text,
    );
    assert_eq!(said, expected);
    // cargo's `FromCompiler` carries the target, not the unit's mode: the same unit compiled
    // with `--test` writes the same message, its `"test"` the target's own.
    let tested = UnitOf { test: true, ..unit.clone() };
    assert_eq!(message(&tested, &unused(rendered), false, &mut sources), expected);
    // A coded error carries rustc's markdown for its code as the code's `explanation`.
    let coded = Diagnostic {
        code: Some("E0384".to_owned()),
        explanation: Some("A variable was assigned twice.\n\n```compile_fail,E0384\n```\n".to_owned()),
        ..unused(rendered)
    };
    assert!(
        message(&unit, &coded, false, &mut sources).ends_with(
            r#""code":{"code":"E0384","explanation":"A variable was assigned twice.\n\n```compile_fail,E0384\n```\n"}}}"#
        )
    );
}

#[test]
fn a_compiler_message_in_color_is_rustcs_as_it_wrote_it() {
    let (_dir, root) = checkout();
    let unit = UnitOf {
        id: 0,
        package_id: format!("path+file://{root}#0.1.0"),
        manifest_path: format!("{root}/Cargo.toml"),
        target: bin_target(&root),
        test: false,
    };
    let rendered = "\u{1b}[33mwarning\u{1b}[0m\n";
    let mut sources = Sources::new(std::path::Path::new(&root));
    let said = message(&unit, &unused(rendered), true, &mut sources);
    let message = said.split_once(r#""message":"#).unwrap().1;
    let text = r#"[{"text":"    let x = 1;","highlight_start":9,"highlight_end":10}]"#;
    let expected = format!(
        concat!(
            r#"{{"$message_type":"diagnostic","message":"unused variable: `x`","code":{{"code":"unused_variables","explanation":null}},"level":"warning","#,
            r#""spans":[{{"file_name":"src/main.rs","byte_start":20,"byte_end":21,"line_start":2,"line_end":2,"column_start":9,"column_end":10,"is_primary":true,"text":{text},"label":null,"suggested_replacement":null,"suggestion_applicability":null,"expansion":null}}],"#,
            r#""children":[{{"message":"if this is intentional, prefix it with an underscore","code":null,"level":"help","#,
            r#""spans":[{{"file_name":"src/main.rs","byte_start":20,"byte_end":21,"line_start":2,"line_end":2,"column_start":9,"column_end":10,"is_primary":true,"text":{text},"label":null,"suggested_replacement":"_x","suggestion_applicability":"MachineApplicable","expansion":null}}],"children":[],"rendered":null}}],"#,
            r#""rendered":"\u001b[33mwarning\u001b[0m\n"}}}}"#,
        ),
        text = text,
    );
    assert_eq!(message, expected);
}

#[test]
fn a_span_over_lines_and_one_of_no_file() {
    let (_dir, root) = checkout();
    let mut sources = Sources::new(std::path::Path::new(&root));
    let file = sources.file("src/main.rs", &root);
    assert!(file.is_some());
    assert_eq!(sources.file("src/main.rs", &root), file, "a name seen is not looked for again");
    let over = sent(&span("src/main.rs", (1, 11), (3, 2), (10, 28)));
    assert_eq!(
        line(|out| span_text(out, lent(&over), Order::Rustc, &sources, file)),
        r#"[{"text":"fn main() {","highlight_start":11,"highlight_end":12},{"text":"    let x = 1;","highlight_start":1,"highlight_end":15},{"text":"}","highlight_start":1,"highlight_end":2}]"#
    );
    let beyond = sent(&span("src/main.rs", (4, 1), (9, 1), (0, 0)));
    assert_eq!(line(|out| span_text(out, lent(&beyond), Order::Rustc, &sources, file)), "[]");
    assert_eq!(sources.file("src/gone.rs", &root), None);
    let absolute = format!("{root}/src/main.rs");
    let same = sources.file(&absolute, "/elsewhere");
    assert_eq!(same, file, "one file read once under two names");
    assert_eq!(sources.line(same.unwrap(), 1), "fn main() {");
    assert_eq!(sources.line_count(same.unwrap()), 4);
}

#[test]
fn lines_without_bom_and_carriage_returns() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), b"\xef\xbb\xbfa\r\nb\n\nc").unwrap();
    std::fs::write(dir.path().join("b.rs"), b"x\n").unwrap();
    let root = dir.path().display().to_string();
    let mut sources = Sources::new(dir.path());
    let a = sources.file("a.rs", &root).unwrap();
    let b = sources.file("b.rs", &root).unwrap();
    let lines: Vec<&str> = (1..=sources.line_count(a)).map(|it| sources.line(a, it)).collect();
    assert_eq!(lines, ["a", "b", "", "c"]);
    let lines: Vec<&str> = (1..=sources.line_count(b)).map(|it| sources.line(b, it)).collect();
    assert_eq!(lines, ["x", ""], "a second file's lines are its own");
}

#[test]
fn a_file_not_utf8_as_rustc_reads_it() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), b"a\xff\nb").unwrap();
    let mut sources = Sources::new(dir.path());
    let a = sources.file("a.rs", &dir.path().display().to_string()).unwrap();
    assert_eq!(sources.line(a, 1), "a\u{fffd}");
    assert_eq!(sources.line(a, 2), "b");
}

/// Once the buffer has grown and the source is read, a compiler message allocates nothing.
#[test]
fn a_compiler_message_warm_allocates_nothing() {
    let (_dir, root) = checkout();
    let unit = sent(&UnitOf {
        id: 0,
        package_id: format!("path+file://{root}#0.1.0"),
        manifest_path: format!("{root}/Cargo.toml"),
        target: bin_target(&root),
        test: false,
    });
    let diagnostic = sent(&unused("\u{1b}[33mwarning\u{1b}[0m: unused variable: `x`\n"));
    let (unit, diagnostic): (UnitOfRef<'_>, DiagnosticRef<'_>) = (lent(&unit), lent(&diagnostic));
    let mut sources = Sources::new(std::path::Path::new(&root));
    let mut out = String::with_capacity(16 * 1024);
    for ansi in [false, true] {
        compiler_message(&mut out, unit, diagnostic, ansi, &mut sources);
        out.clear();
        blaze_alloc_count::assert_at_most(0, "a warm compiler-message", || {
            compiler_message(&mut out, unit, diagnostic, ansi, &mut sources);
            out.clear();
        });
    }
}

/// The escape's stripping of colors against the reference: `say::strip_ansi`, then serde_json.
/// The 8-byte scan finds every byte that is escaped, wherever in a word it falls, and none
/// that isn't: each ASCII byte and a multi-byte character at every offset of 24 plain bytes.
#[test]
fn every_byte_escaped_as_serde_json_wherever_it_falls() {
    let mut cases: Vec<String> = Vec::new();
    for at in 0..24 {
        for byte in 0u8..0x80 {
            let mut text = "a".repeat(24);
            text.replace_range(at..at + 1, &char::from(byte).to_string());
            cases.push(text);
        }
        let mut text = "a".repeat(24);
        text.insert(at, 'é');
        cases.push(text);
    }
    for text in cases {
        let mut quoted = String::new();
        quote(&mut quoted, &text);
        assert_eq!(quoted, serde_json::to_string(&text).unwrap(), "{text:?}");
    }
}

#[test]
fn colors_stripped_while_escaping() {
    for text in [
        "\u{1b}[1m\u{1b}[33mwarning\u{1b}[0m: unused `x`\n",
        "a\u{1b}]8;;https://x\u{7}link\u{1b}]8;;\u{1b}\\b",
        "\u{1b}(é\t\"q\"\\",
        "unterminated \u{1b}[38;5",
        "\u{1b}",
        "plain",
    ] {
        let reference = serde_json::to_string(&crate::cli::say::strip_ansi(text)).unwrap();
        assert_eq!(line(|out| quote_with(out, text, true)), reference, "{text:?}");
    }
}

fn line(write: impl FnOnce(&mut String)) -> String {
    let mut out = String::new();
    write(&mut out);
    out
}
