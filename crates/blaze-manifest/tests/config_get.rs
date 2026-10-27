//! Each tree of `tests/config-fixtures` read as `cargo -Zunstable-options config get` with the
//! arguments of each run, against what cargo 1.99 printed (`expected.json`, written by
//! `harness/resolve.py --fixtures` with the runs of the case's `case.json` after its
//! `CONFIG_RUNS`).

// `expected.json` is what cargo printed on Linux: `/` in the paths of the files it read and
// POSIX shell quoting. On Windows cargo prints `\` there, which these were not recorded with.
#![cfg(unix)]

use std::path::{Path, PathBuf};

use blaze_manifest::config::get::main as config_main;

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if name == "expected.json" || name == "case.json" {
            continue;
        }
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to.join(&name));
        } else {
            std::fs::copy(entry.path(), to.join(&name)).unwrap();
        }
    }
}

/// The differences between what blaze-manifest prints for each run of the case and what cargo did.
fn check(fixture: &Path, scratch: &Path) -> Vec<String> {
    let root = scratch.join(fixture.file_name().unwrap());
    copy_dir(fixture, &root);
    let root = root.canonicalize().unwrap();
    let case: serde_json::Value = match std::fs::read_to_string(fixture.join("case.json")) {
        Ok(text) => serde_json::from_str(&text).unwrap(),
        Err(_) => serde_json::json!({}),
    };
    let home = match case["home"].as_str() {
        Some(home) => root.join(home),
        None => {
            let home = scratch.join("cargo-home");
            std::fs::create_dir_all(&home).unwrap();
            home.canonicalize().unwrap()
        }
    };
    let root_text = root.display().to_string();
    // Stop the walk up for `.cargo/config.toml` at the scratch dir, as cargo's own tests do, so
    // config files above it (an ix VM's `/.cargo/config.toml`) don't reach what cargo recorded.
    let test_root = scratch.canonicalize().unwrap();
    let mut env = vec![
        ("CARGO_HOME".to_owned(), home.display().to_string()),
        ("__CARGO_TEST_ROOT".to_owned(), test_root.display().to_string()),
    ];
    for (k, v) in case["env"].as_object().into_iter().flatten() {
        env.push((k.clone(), v.as_str().unwrap().replace("$ROOT", &root_text)));
    }
    let cwd = root.join(case["cwd"].as_str().unwrap_or(""));
    let normalize = |text: String| {
        text.replace(&home.display().to_string(), "$CARGO_HOME").replace(&root_text, "$ROOT")
    };
    let expected: Vec<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(fixture.join("expected.json")).unwrap())
            .unwrap();
    let mut failed = Vec::new();
    for run in &expected {
        let args: Vec<String> = run["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap().to_owned())
            .collect();
        let mut full = vec!["-Zunstable-options".to_owned(), "config".to_owned(), "get".to_owned()];
        full.extend(args.iter().cloned());
        let (code, output) = config_main(&cwd, env.clone(), &full);
        let got = [
            ("code", code.to_string()),
            ("stdout", normalize(output.stdout)),
            ("stderr", normalize(output.stderr)),
        ];
        for (stream, text) in got {
            let want = match &run[stream] {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            if text != want {
                failed.push(format!(
                    "{} `config get {}`: {stream}: blaze\n{text}\ncargo\n{want}",
                    fixture.file_name().unwrap().to_string_lossy(),
                    args.join(" ")
                ));
                break;
            }
        }
    }
    failed
}

#[test]
fn config_fixtures_read_as_cargo_reads_them() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/config-fixtures");
    let scratch =
        std::env::temp_dir().join(format!("blaze-manifest-config-{}", std::process::id()));
    let mut names: Vec<_> =
        std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
    names.sort();
    let mut failed = Vec::new();
    for name in &names {
        failed.extend(check(&dir.join(name), &scratch));
    }
    let _ = std::fs::remove_dir_all(&scratch);
    assert!(failed.is_empty(), "{}", failed.join("\n\n"));
    assert!(names.len() >= 10);
}
