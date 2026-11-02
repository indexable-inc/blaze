use std::path::PathBuf;

use super::*;
use crate::config::Verbosity;

fn config(mode: EmitMode) -> Config {
    let mut config = Config::default();
    config.set().emit_mode(mode);
    config
}

fn emit(emitter: &mut Emitter, original: &str, formatted: &str) -> (EmitterResult, String) {
    let filename = FileName::Real(PathBuf::from("/blaze-fmt-emitter-test/a.rs"));
    let mut out = Vec::new();
    emitter.emit_header(&mut out).unwrap();
    let result = emitter
        .emit_formatted_file(
            &mut out,
            FormattedFile {
                filename: &filename,
                original_text: original,
                formatted_text: formatted,
            },
        )
        .unwrap();
    emitter.emit_footer(&mut out).unwrap();
    (result, String::from_utf8(out).unwrap())
}

#[test]
fn files_unchanged_writes_nothing() {
    let mut emitter = Emitter::new(&config(EmitMode::Files));
    assert!(matches!(emitter, Emitter::Files(_)));
    let (result, out) = emit(&mut emitter, "fn f() {}\n", "fn f() {}\n");
    assert!(!result.has_diff);
    assert_eq!(out, "");
}

#[test]
fn files_with_backup_unchanged_writes_nothing() {
    let mut config = config(EmitMode::Files);
    config.set().make_backup(true);
    let mut emitter = Emitter::new(&config);
    assert!(matches!(emitter, Emitter::FilesWithBackup(_)));
    let (result, out) = emit(&mut emitter, "fn f() {}\n", "fn f() {}\n");
    assert!(!result.has_diff);
    assert_eq!(out, "");
}

#[test]
fn stdout_and_coverage_print_the_text() {
    for mode in [EmitMode::Stdout, EmitMode::Coverage] {
        let mut config = config(mode);
        config.set().verbose(Verbosity::Quiet);
        let mut emitter = Emitter::new(&config);
        assert!(matches!(emitter, Emitter::Stdout(_)));
        let (result, out) = emit(&mut emitter, "fn f(){}\n", "fn f() {}\n");
        assert!(!result.has_diff);
        assert_eq!(out, "fn f() {}\n");
    }
}

#[test]
fn json_collects_until_the_footer() {
    let mut emitter = Emitter::new(&config(EmitMode::Json));
    assert!(matches!(emitter, Emitter::Json(_)));
    let (result, out) = emit(&mut emitter, "fn f(){}\n", "fn f() {}\n");
    assert!(result.has_diff);
    assert!(out.starts_with(r#"[{"name":"/blaze-fmt-emitter-test/a.rs","mismatches":[{"#));
    assert!(out.ends_with("}]\n"));
}

#[test]
fn modified_lines_reports_the_change() {
    let mut emitter = Emitter::new(&config(EmitMode::ModifiedLines));
    assert!(matches!(emitter, Emitter::ModifiedLines(_)));
    let (result, out) = emit(&mut emitter, "fn f(){}\n", "fn f() {}\n");
    assert!(result.has_diff);
    assert!(out.contains("fn f() {}"));
}

#[test]
fn checkstyle_wraps_in_header_and_footer() {
    let mut emitter = Emitter::new(&config(EmitMode::Checkstyle));
    assert!(matches!(emitter, Emitter::Checkstyle(_)));
    let (result, out) = emit(&mut emitter, "fn f(){}\n", "fn f() {}\n");
    assert!(!result.has_diff);
    assert!(out.starts_with(r#"<?xml version="1.0" encoding="utf-8"?>"#));
    assert!(out.contains(r#"<file name="/blaze-fmt-emitter-test/a.rs">"#));
    assert!(out.contains(r#"message="Should be `fn f() {}`""#));
    assert!(out.ends_with("</checkstyle>\n"));
}

#[test]
fn diff_names_the_misformatted_file() {
    let mut config = config(EmitMode::Diff);
    config.set().print_misformatted_file_names(true);
    let mut emitter = Emitter::new(&config);
    assert!(matches!(emitter, Emitter::Diff(_)));
    let (result, out) = emit(&mut emitter, "fn f(){}\n", "fn f() {}\n");
    assert!(result.has_diff);
    assert_eq!(out, "/blaze-fmt-emitter-test/a.rs\n");

    let (result, out) = emit(&mut emitter, "fn f() {}\n", "fn f() {}\n");
    assert!(!result.has_diff);
    assert_eq!(out, "");
}
