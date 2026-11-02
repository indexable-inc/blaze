// blaze: the one LLVM blaze links (crates/hir-ty-codegen/llvm.json), checked before anything of
// LLVM is run or linked. vendor/llvm-sys/build.rs includes this file and every LLVM consumer in
// blaze links LLVM through llvm-sys, so none can skip it; llvm-sys's `pin` test target is this
// file too. Std only and no `use`, so it includes into any build script.

/// The one variable naming the prefix: no other `LLVM_SYS_*_PREFIX`, no `PATH`, no Homebrew.
const PIN_PREFIX: &str = "LLVM_SYS_231_PREFIX";
/// What `.github/scripts/llvm.py build` writes into each prefix it makes: its version and commit.
const PIN_MARKER: &str = "blaze-llvm.json";
/// The pin, from llvm-sys's manifest directory.
const PIN_JSON: &str = "../../crates/hir-ty-codegen/llvm.json";

/// The LLVM llvm.json pins.
#[derive(Debug, PartialEq, Eq)]
struct Pin {
    version: String,
    commit: String,
}

impl Pin {
    fn parse(json: &str) -> Result<Pin, String> {
        Ok(Pin { version: pin_field(json, "version")?, commit: pin_field(json, "commit")? })
    }

    /// The one message that stops the build: what was `found`, the pin and the fix. One line,
    /// as `cargo::error=` takes.
    fn refusal(&self, found: &str) -> String {
        format!(
            "blaze links only the LLVM crates/hir-ty-codegen/llvm.json pins (LLVM {}, commit {}), \
             and {found}. Build inside `nix develop`, which sets {PIN_PREFIX} to the flake's \
             blaze-llvm, or unpack its release with `python3 .github/scripts/llvm.py fetch \
             --target <triple> --dir <dir>` and set {PIN_PREFIX} to <dir>; for a target llvm.json \
             has no release for, build one with the `llvm` workflow (.github/workflows/llvm.yaml)",
            self.version, self.commit
        )
    }
}

/// Runs first in the build script: the pinned prefix's `llvm-config`, or `None` once the one
/// `cargo::error` saying why is printed, and the build script then stops.
#[cfg_attr(test, allow(dead_code))]
fn pinned_llvm_config() -> Option<std::path::PathBuf> {
    println!("cargo::rerun-if-env-changed={PIN_PREFIX}");
    let manifest = std::path::PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"),
    );
    let json = manifest.join(PIN_JSON);
    println!("cargo::rerun-if-changed={}", json.display());
    let prefix = std::env::var_os(PIN_PREFIX);
    if let Some(prefix) = prefix.as_ref().filter(|it| !it.is_empty()) {
        println!("cargo::rerun-if-changed={}", std::path::Path::new(prefix).join(PIN_MARKER).display());
    }
    let pin = match std::fs::read_to_string(&json)
        .map_err(|e| e.to_string())
        .and_then(|it| Pin::parse(&it))
    {
        Ok(pin) => pin,
        Err(e) => {
            println!("cargo::error=blaze's LLVM pin `{}` can't be read: {e}", json.display());
            return None;
        }
    };
    match pin_check(&pin, prefix.as_deref(), std::env::consts::EXE_SUFFIX) {
        Ok(config) => Some(config),
        Err(found) => {
            println!("cargo::error={}", pin.refusal(&found));
            None
        }
    }
}

/// `prefix`'s `llvm-config` when `prefix` is the pinned build (its marker names the pinned
/// version and commit, and its `llvm-config` says that version), else what was found there.
/// `exe` is the executable suffix of the host, which runs `llvm-config`.
fn pin_check(
    pin: &Pin,
    prefix: Option<&std::ffi::OsStr>,
    exe: &str,
) -> Result<std::path::PathBuf, String> {
    let prefix = match prefix {
        Some(prefix) if !prefix.is_empty() => std::path::Path::new(prefix),
        _ => return Err(format!("{PIN_PREFIX} is unset")),
    };
    let config = prefix.join("bin").join(format!("llvm-config{exe}"));
    let marker = prefix.join(PIN_MARKER);
    let built = match std::fs::read_to_string(&marker) {
        Ok(it) => it,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let what = match pin_said(&config) {
                Ok(said) => format!("a stock LLVM {said}"),
                Err(_) => "no LLVM with a working bin/llvm-config".to_owned(),
            };
            return Err(format!(
                "{PIN_PREFIX} is `{}`, which holds {what} and no {PIN_MARKER}, so not blaze's \
                 build at the pinned commit",
                prefix.display()
            ));
        }
        Err(e) => return Err(format!("`{}` can't be read: {e}", marker.display())),
    };
    let field = |key: &str| pin_field(&built, key).map_err(|e| format!("`{}`: {e}", marker.display()));
    let (version, commit) = (field("version")?, field("commit")?);
    if version != pin.version || commit != pin.commit {
        return Err(format!(
            "{PIN_PREFIX} is `{}`, a build of LLVM {version} at commit {commit}",
            prefix.display()
        ));
    }
    let said = pin_said(&config)
        .map_err(|e| format!("{PIN_PREFIX} is `{}`, and {e}", prefix.display()))?;
    if said != pin.version {
        return Err(format!(
            "{PIN_PREFIX} is `{}`, whose `{}` says LLVM {said}",
            prefix.display(),
            config.display()
        ));
    }
    Ok(config)
}

/// What `config --version` says, trimmed, or why it said nothing (on one line).
fn pin_said(config: &std::path::Path) -> Result<String, String> {
    let out = std::process::Command::new(config)
        .arg("--version")
        .output()
        .map_err(|e| format!("`{} --version` did not run: {e}", config.display()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "`{} --version` failed ({}): {}",
            config.display(),
            out.status,
            stderr.trim().replace('\n', "; ")
        ));
    }
    String::from_utf8(out.stdout)
        .map(|it| it.trim().to_owned())
        .map_err(|_| format!("`{} --version` printed no UTF-8", config.display()))
}

/// The string value of `"key": "value"` in `json`. The keys asked for are top-level and unique
/// in llvm.json and the marker, and their values hold no quote or escape.
fn pin_field(json: &str, key: &str) -> Result<String, String> {
    let quoted = format!("\"{key}\"");
    let mut from = 0;
    while let Some(at) = json[from..].find(&quoted) {
        from += at + quoted.len();
        let Some(rest) = json[from..].trim_start().strip_prefix(':') else {
            continue;
        };
        let value = rest
            .trim_start()
            .strip_prefix('"')
            .ok_or_else(|| format!("\"{key}\" is not a string"))?;
        let end = value
            .find(['"', '\\'])
            .ok_or_else(|| format!("\"{key}\" has no closing quote"))?;
        if value.as_bytes()[end] == b'\\' {
            return Err(format!("\"{key}\" holds an escape"));
        }
        return Ok(value[..end].to_owned());
    }
    Err(format!("no \"{key}\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LLVM_JSON: &str = include_str!("../../crates/hir-ty-codegen/llvm.json");

    fn pin() -> Pin {
        Pin::parse(LLVM_JSON).expect("llvm.json parses")
    }

    #[test]
    fn reads_the_pin() {
        let pin = pin();
        assert_eq!(pin.version.split('.').count(), 3, "{pin:?}");
        assert!(pin.version.split('.').all(|it| it.parse::<u32>().is_ok()), "{pin:?}");
        assert_eq!(pin.commit.len(), 40, "{pin:?}");
        assert!(pin.commit.bytes().all(|b| b.is_ascii_hexdigit()), "{pin:?}");
    }

    #[test]
    fn field_skips_a_key_used_as_a_value() {
        let json = r#"{"ref": "version", "version" : "1.2.3", "commit":"abc"}"#;
        assert_eq!(pin_field(json, "version").as_deref(), Ok("1.2.3"));
        assert_eq!(pin_field(json, "commit").as_deref(), Ok("abc"));
        assert_eq!(pin_field(json, "ref").as_deref(), Ok("version"));
    }

    #[test]
    fn field_refuses_what_it_cannot_read() {
        assert_eq!(pin_field("{}", "commit"), Err("no \"commit\"".to_owned()));
        assert!(pin_field(r#"{"commit": 1}"#, "commit").is_err());
        assert!(pin_field(r#"{"commit": "ab"#, "commit").is_err());
        assert!(pin_field(r#"{"commit": "a\"b"}"#, "commit").is_err());
    }

    #[test]
    fn refusal_names_the_pin_and_the_fix_on_one_line() {
        let pin = pin();
        let message = pin.refusal("something else");
        for needle in [
            pin.version.as_str(),
            pin.commit.as_str(),
            "something else",
            "nix develop",
            PIN_PREFIX,
            ".github/workflows/llvm.yaml",
        ] {
            assert!(message.contains(needle), "{needle} not in {message}");
        }
        assert!(!message.contains('\n'), "{message}");
    }

    #[test]
    fn refuses_an_unset_prefix() {
        let unset = format!("{PIN_PREFIX} is unset");
        assert_eq!(pin_check(&pin(), None, ""), Err(unset.clone()));
        assert_eq!(pin_check(&pin(), Some(std::ffi::OsStr::new("")), ""), Err(unset));
    }

    /// One test for every prefix on disk, run in order: a script written and run while another
    /// test thread forks can fail with ETXTBSY.
    #[test]
    fn checks_a_prefix_on_disk() {
        let pin = pin();
        let dir = std::env::temp_dir().join(format!("blaze-llvm-pin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        let check = || pin_check(&pin, Some(dir.as_os_str()), "");
        let marker = |version: &str, commit: &str| {
            let json = format!(
                "{{\n  \"version\": \"{version}\",\n  \"repo\": \"r\",\n  \"ref\": \"x\",\n  \
                 \"commit\": \"{commit}\",\n  \"target\": \"t\"\n}}\n"
            );
            std::fs::write(dir.join(PIN_MARKER), json).unwrap();
        };

        let err = check().unwrap_err();
        assert!(err.contains("no LLVM with a working bin/llvm-config"), "{err}");
        assert!(err.contains(PIN_MARKER), "{err}");

        marker(&pin.version, "0000000000000000000000000000000000000000");
        let err = check().unwrap_err();
        assert!(err.contains("at commit 0000000000000000000000000000000000000000"), "{err}");

        marker("1.0.0", &pin.commit);
        let err = check().unwrap_err();
        assert!(err.contains("LLVM 1.0.0"), "{err}");

        marker(&pin.version, &pin.commit);
        let err = check().unwrap_err();
        assert!(err.contains("did not run"), "{err}");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let config = dir.join("bin").join("llvm-config");
            let says = |version: &str| {
                std::fs::write(&config, format!("#!/bin/sh\necho {version}\n")).unwrap();
                std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o755)).unwrap();
            };

            says(&pin.version);
            assert_eq!(check(), Ok(config.clone()));

            says("22.1.0");
            let err = check().unwrap_err();
            assert!(err.contains("says LLVM 22.1.0"), "{err}");

            says(&pin.version);
            std::fs::remove_file(dir.join(PIN_MARKER)).unwrap();
            let err = check().unwrap_err();
            assert!(err.contains(&format!("a stock LLVM {}", pin.version)), "{err}");
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
