// blaze: the one LLVM blaze links (crates/hir-ty-codegen/llvm.json), checked before anything of
// LLVM is run or linked. vendor/llvm-sys/build.rs includes this file and every LLVM consumer in
// blaze links LLVM through llvm-sys, so none can skip it; llvm-sys's `pin` test target
// (tests/pin.rs) includes it with its tests, and blaze-codegen-code includes it to key the code
// identity by the pin. Its tests live in tests/pin.rs, not here: one of them writes and runs an
// `llvm-config` script, which can fail with ETXTBSY while another test thread forks, so it must
// stay in a binary of its own and out of every includer's. Std only and no `use`, so it includes
// into any build script.

/// The one variable naming the prefix: no other `LLVM_SYS_*_PREFIX`, no `PATH`, no Homebrew.
pub(crate) const PIN_PREFIX: &str = "LLVM_SYS_231_PREFIX";
/// What `.github/scripts/llvm.py build` writes into each prefix it makes: its version and commit.
pub(crate) const PIN_MARKER: &str = "blaze-llvm.json";
/// The pin, from llvm-sys's manifest directory.
pub(crate) const PIN_JSON: &str = "../../crates/hir-ty-codegen/llvm.json";

/// The LLVM llvm.json pins.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Pin {
    pub(crate) version: String,
    pub(crate) commit: String,
}

impl Pin {
    pub(crate) fn parse(json: &str) -> Result<Pin, String> {
        Ok(Pin { version: pin_field(json, "version")?, commit: pin_field(json, "commit")? })
    }

    /// The one message that stops the build: what was `found`, the pin and the fix. One line,
    /// as `cargo::error=` takes.
    pub(crate) fn refusal(&self, found: &str) -> String {
        format!(
            "blaze links only the LLVM crates/hir-ty-codegen/llvm.json pins (LLVM {}, commit {}), \
             and {found}. blaze sets {PIN_PREFIX} itself, to the release llvm.json pins for the \
             target, fetched by its sha256 and unpacked under blaze's home \
             (`[workspace.metadata.blaze.native.llvm]` in the root Cargo.toml, docs/cargo.md \
             \"Pinned native inputs\"); build with blaze. For a target llvm.json has no release \
             for, build one with the `llvm` workflow (.github/workflows/llvm.yaml)",
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
    let text = std::fs::read_to_string(&json).map_err(|e| e.to_string());
    let pin = match text.as_deref().map_err(String::clone).and_then(Pin::parse) {
        Ok(pin) => pin,
        Err(e) => {
            println!("cargo::error=blaze's LLVM pin `{}` can't be read: {e}", json.display());
            return None;
        }
    };
    match pin_check(&pin, prefix.as_deref(), std::env::consts::EXE_SUFFIX) {
        Ok(config) => Some(config),
        Err(found) => {
            pin_report(&pin, text.as_deref().unwrap_or_default(), prefix.as_deref(), &found);
            println!("cargo::error={}", pin.refusal(&found));
            None
        }
    }
}

/// The refusal again as `cargo:blaze-report=` lines (crates/blaze-report/src/script.rs), which
/// blaze shows as its report: the cause in a line, what is needed and found, and fixes filled
/// in for this target. cargo keeps them as metadata and shows the `cargo::error` alone.
#[cfg_attr(test, allow(dead_code))]
fn pin_report(pin: &Pin, json: &str, prefix: Option<&std::ffi::OsStr>, found: &str) {
    let say = |field: &str, text: &str| println!("cargo:blaze-report={field}={}", text.replace(['\n', '\r'], " "));
    let unset = prefix.is_none_or(|it| it.is_empty());
    match unset {
        true => say("headline", "llvm-sys can't find LLVM"),
        false => say("headline", &format!("llvm-sys can't use the LLVM `{PIN_PREFIX}` names")),
    }
    say("rule", "llvm:pin");
    let commit = &pin.commit[..pin.commit.len().min(8)];
    say("fact.needs", &format!("needs LLVM {} ({commit}), pinned in crates/hir-ty-codegen/llvm.json", pin.version));
    match unset {
        true => say("fact.found", &format!("`{PIN_PREFIX}` is not set")),
        false => say("fact.found", found),
    }
    say("fix", "nix develop");
    let target = std::env::var("TARGET").unwrap_or_default();
    match pin_sha256(json, &target) {
        Ok(Some(_)) => {
            say(
                "fix",
                &format!(
                    r#"python3 .github/scripts/llvm.py '{{"command": "fetch", "target": "{target}", "dir": "~/.blaze/llvm"}}'"#
                ),
            );
            say("fix+", &format!("export {PIN_PREFIX}=~/.blaze/llvm"));
        }
        Ok(None) | Err(_) => say(
            "fact.release",
            &format!("llvm.json has no release for `{target}`: build one with the `llvm` workflow (.github/workflows/llvm.yaml)"),
        ),
    }
}

/// `prefix`'s `llvm-config` when `prefix` is the pinned build (its marker names the pinned
/// version and commit, and its `llvm-config` says that version), else what was found there.
/// `exe` is the executable suffix of the host, which runs `llvm-config`.
pub(crate) fn pin_check(
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
pub(crate) fn pin_field(json: &str, key: &str) -> Result<String, String> {
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

/// The `sha256` llvm.json pins for `target`'s release, or `None` when it has no release for
/// it. A target's entry is an object of strings that hold no brace.
#[cfg_attr(not(test), allow(dead_code, reason = "llvm-sys's build script keys nothing by it; blaze-codegen-code does"))]
pub(crate) fn pin_sha256(json: &str, target: &str) -> Result<Option<String>, String> {
    let targets = json.find("\"targets\"").ok_or("no \"targets\"")?;
    let quoted = format!("\"{target}\"");
    let Some(at) = json[targets..].find(&quoted) else { return Ok(None) };
    let rest = json[targets + at + quoted.len()..].trim_start();
    let entry = rest
        .strip_prefix(':')
        .map(str::trim_start)
        .and_then(|it| it.strip_prefix('{'))
        .ok_or_else(|| format!("\"{target}\" is not an object"))?;
    let end = entry.find('}').ok_or_else(|| format!("\"{target}\" has no closing brace"))?;
    pin_field(&entry[..end], "sha256").map(Some).map_err(|e| format!("\"{target}\": {e}"))
}
