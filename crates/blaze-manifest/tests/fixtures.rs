//! Each workspace of `tests/fixtures` read as `cargo metadata --no-deps --format-version 1`
//! from the directory its `fixture.json` names, against what cargo 1.99 printed for it
//! (`expected.json`, written by `harness/resolve.py --fixtures`).

// `expected.json` is what cargo printed on Linux: `/` in its paths and `file:///` ids of
// them. On Windows cargo prints `\` and `file:///C:/` there, which these were not recorded with.
#![cfg(unix)]

use std::path::{Path, PathBuf};

use blaze_manifest::diagnostic::ManifestDiagnostic;
use blaze_manifest::metadata::{export_no_deps, root_manifest};
use blaze_manifest::{Config, Context, Workspace};
use serde::Serialize;

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if name == "expected.json" || name == "fixture.json" {
            continue;
        }
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to.join(&name));
        } else {
            std::fs::copy(entry.path(), to.join(&name)).unwrap();
        }
    }
}

/// The error as cargo prints it, blank lines dropped as `harness/resolve.py` drops them.
fn error_text(err: &anyhow::Error, cwd: &Path) -> String {
    if let Some(diagnostic) = err.downcast_ref::<ManifestDiagnostic>() {
        return diagnostic
            .render(cwd)
            .lines()
            .map(str::trim_end)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
    }
    let mut lines = Vec::new();
    for (i, cause) in err.chain().enumerate() {
        if i == 0 {
            let text = format!("error: {cause}");
            lines.extend(text.lines().filter(|l| !l.is_empty()).map(str::to_owned));
        } else {
            lines.push("Caused by:".to_owned());
            lines.extend(
                cause.to_string().lines().filter(|l| !l.is_empty()).map(|l| format!("  {l}")),
            );
        }
    }
    lines.join("\n")
}

/// What blaze-manifest prints for the fixture, in the form of `expected.json`.
fn read(fixture: &Path, scratch: &Path) -> String {
    let root = scratch.join(fixture.file_name().unwrap());
    copy_dir(fixture, &root);
    let root = root.canonicalize().unwrap();
    let home = scratch.join("cargo-home");
    std::fs::create_dir_all(&home).unwrap();
    let cwd = match std::fs::read_to_string(fixture.join("fixture.json")) {
        Ok(text) => {
            let spec: serde_json::Value = serde_json::from_str(&text).unwrap();
            root.join(spec["cwd"].as_str().unwrap_or(""))
        }
        Err(_) => root.clone(),
    };
    let env = [("CARGO_HOME".to_owned(), home.display().to_string())];
    let result = (|| {
        let ctx = Context::new(Config::load(&cwd, env, &[])?);
        let manifest = root_manifest(None, &ctx)?;
        let ws = Workspace::new(&manifest, &ctx)?;
        let mut out = Vec::new();
        let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
        export_no_deps(&ws)
            .serialize(&mut serde_json::Serializer::with_formatter(&mut out, formatter))?;
        anyhow::Ok(String::from_utf8(out)?)
    })();
    let root = root.display().to_string();
    let home = home.canonicalize().unwrap().display().to_string();
    let text = match result {
        Ok(json) => json,
        Err(err) => {
            let mut out = Vec::new();
            let error = serde_json::json!({ "error": error_text(&err, &cwd) });
            let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
            error
                .serialize(&mut serde_json::Serializer::with_formatter(&mut out, formatter))
                .unwrap();
            String::from_utf8(out).unwrap()
        }
    };
    text.replace(&root, "$ROOT").replace(&home, "$CARGO_HOME") + "\n"
}

#[test]
fn fixtures_read_as_cargo_reads_them() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let scratch =
        std::env::temp_dir().join(format!("blaze-manifest-fixtures-{}", std::process::id()));
    let mut names: Vec<_> =
        std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
    names.sort();
    let mut failed = Vec::new();
    for name in &names {
        let fixture = dir.join(name);
        let expected = std::fs::read_to_string(fixture.join("expected.json")).unwrap();
        let got = read(&fixture, &scratch);
        if got != expected {
            let line = got.lines().zip(expected.lines()).position(|(a, b)| a != b).unwrap_or(0);
            failed.push(format!(
                "{}: line {}: blaze `{}`, cargo `{}`",
                name.to_string_lossy(),
                line + 1,
                got.lines().nth(line).unwrap_or(""),
                expected.lines().nth(line).unwrap_or("")
            ));
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
    assert!(failed.is_empty(), "{}", failed.join("\n"));
    assert!(names.len() >= 4);
}
