//! Each tree of `tests/profile-fixtures` with the unit graph cargo 1.99 printed for each run
//! (`expected.json`, written by `harness/resolve.py --fixtures` with the runs of the case's
//! `case.json` after its `PROFILE_RUNS`): the profile blaze-manifest computes for every unit,
//! the warnings about profiles and the error when cargo refused the profiles, against cargo's.

// `expected.json` is what cargo printed on Linux: its unit graph names Linux paths, put in a
// JSON text as they are, which a Windows path's `\` is not.
#![cfg(unix)]
#![allow(dead_code, reason = "the example's module, of which the test uses `main`")]

#[path = "../examples/profiles/walk.rs"]
mod walk;

use std::path::Path;

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

/// The error of `stderr` as `harness/resolve.py`'s `error_text` takes it.
fn error_text(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.lines().collect();
    let start = lines.iter().position(|l| l.starts_with("error")).unwrap_or(lines.len());
    lines[start..]
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A warning less the manifest it is about, as `harness/resolve.py`'s `profile_warnings` takes it.
fn message(line: &str) -> &str {
    let rest = &line["warning: ".len()..];
    match rest.split_once(": ") {
        Some((path, message)) if !path.contains(' ') && path.ends_with("Cargo.toml") => message,
        _ => rest,
    }
}

fn strings(value: &serde_json::Value) -> Vec<String> {
    value.as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_owned()).collect()
}

/// The differences between what blaze-manifest computes for each run of the case and cargo's.
fn check(fixture: &Path, scratch: &Path) -> (usize, Vec<String>) {
    let name = fixture.file_name().unwrap().to_string_lossy().into_owned();
    let root = scratch.join(&name);
    copy_dir(fixture, &root);
    let root = root.canonicalize().unwrap();
    let home = scratch.join("cargo-home");
    std::fs::create_dir_all(&home).unwrap();
    let home = home.canonicalize().unwrap();
    let case: serde_json::Value = match std::fs::read_to_string(fixture.join("case.json")) {
        Ok(text) => serde_json::from_str(&text).unwrap(),
        Err(_) => serde_json::json!({}),
    };
    let cwd = root.join(case["cwd"].as_str().unwrap_or(""));
    let root_text = root.display().to_string();
    let normalize = |text: &str| {
        text.replace(&home.display().to_string(), "$CARGO_HOME").replace(&root_text, "$ROOT")
    };
    let expected: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(fixture.join("expected.json")).unwrap())
            .unwrap();
    let host = expected["host"].as_str().unwrap();
    let runs = expected["runs"].as_array().unwrap();
    let mut failed = Vec::new();
    for run in runs {
        let args = strings(&run["args"]);
        let mut env = vec![("CARGO_HOME".to_owned(), home.display().to_string())];
        for (k, v) in run["env"].as_object().into_iter().flatten() {
            env.push((k.clone(), v.as_str().unwrap().replace("$ROOT", &root_text)));
        }
        let graph = match &run["graph"] {
            serde_json::Value::Null => String::new(),
            graph => graph.to_string().replace("$ROOT", &root_text),
        };
        let output = walk::main(&cwd, env, host, &args, &graph);
        let what = format!("{name} `{}`", args.join(" "));
        let mut warnings: Vec<String> = output
            .stderr
            .lines()
            .filter(|l| l.starts_with("warning: ") && message(l).contains("profile"))
            .map(normalize)
            .collect();
        warnings.sort();
        if warnings != strings(&run["warnings"]) {
            failed.push(format!("{what}: warnings {warnings:?}, cargo's {}", run["warnings"]));
        }
        match run["code"].as_u64().unwrap() {
            0 if output.code != 0 => {
                failed.push(format!("{what}: blaze failed: {}", output.stderr));
            }
            0 => {
                let got: Vec<serde_json::Value> = serde_json::from_str(&output.stdout).unwrap();
                let want = run["graph"]["units"].as_array().unwrap();
                for (i, unit) in want.iter().enumerate() {
                    if got.get(i) != Some(&unit["profile"]) {
                        failed.push(format!(
                            "{what}: unit {i} ({} {} {}): blaze {:?}, cargo {}",
                            unit["pkg_id"],
                            unit["target"]["name"],
                            unit["mode"],
                            got.get(i),
                            unit["profile"]
                        ));
                    }
                }
            }
            _ if output.code == 0 => failed.push(format!("{what}: cargo failed, blaze did not")),
            _ => {
                let error = normalize(&error_text(&output.stderr));
                if error != run["error"].as_str().unwrap() {
                    failed.push(format!("{what}: error\n{error}\ncargo's\n{}", run["error"]));
                }
            }
        }
    }
    (runs.len(), failed)
}

#[test]
fn profiles_match_cargo() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/profile-fixtures");
    let scratch =
        std::env::temp_dir().join(format!("blaze-manifest-profiles-{}", std::process::id()));
    let mut names: Vec<_> =
        std::fs::read_dir(&fixtures).unwrap().map(|e| e.unwrap().file_name()).collect();
    names.sort();
    let mut runs = 0;
    let mut failed = Vec::new();
    for name in &names {
        let (n, f) = check(&fixtures.join(name), &scratch.join(name));
        runs += n;
        failed.extend(f);
    }
    let _ = std::fs::remove_dir_all(&scratch);
    assert!(runs >= 40, "only {runs} runs in {}", fixtures.display());
    assert!(failed.is_empty(), "{} differences:\n{}", failed.len(), failed.join("\n\n"));
}
