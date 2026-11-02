//! The reading of a script's output before its single borrowed pass: a `strip_prefix` per
//! syntax, a `String` per value before its key was matched, and a string `match` of the key.
//! Kept as the reference [`crate::output::parse`] is checked against over seeded outputs.

use std::path::PathBuf;

use crate::output::{
    Context, DOCS, Descr, Error, LibraryPath, LinkArgTarget, Output, Severity, Whence, directive,
    key_value, new_syntax_refused, older_than_new_syntax,
};

fn reserved(key: &str) -> bool {
    matches!(
        key,
        "rustc-flags"
            | "rustc-link-lib"
            | "rustc-link-search"
            | "rustc-link-arg-cdylib"
            | "rustc-cdylib-link-arg"
            | "rustc-link-arg-bins"
            | "rustc-link-arg-bin"
            | "rustc-link-arg-tests"
            | "rustc-link-arg-benches"
            | "rustc-link-arg-examples"
            | "rustc-link-arg"
            | "rustc-cfg"
            | "rustc-check-cfg"
            | "rustc-env"
            | "warning"
            | "rerun-if-changed"
            | "rerun-if-env-changed"
    )
}

fn parse_rustc_flags(
    value: &str,
    whence: impl std::fmt::Display,
) -> Result<(Vec<PathBuf>, Vec<String>), Error> {
    let mut flags = value.trim().split(char::is_whitespace).filter(|it| !it.trim().is_empty());
    let (mut paths, mut links) = (Vec::new(), Vec::new());
    while let Some(flag) = flags.next() {
        if !flag.starts_with("-l") && !flag.starts_with("-L") {
            return Err(Error(format!(
                "only `-l` and `-L` flags are allowed in {whence}: `{}`",
                value
            )));
        }
        let (flag, mut rest) = flag.split_at(2);
        if rest.is_empty() {
            rest = flags.next().ok_or_else(|| {
                Error(format!("flag in rustc-flags has no value in {whence}: {rest}"))
            })?;
        }
        match flag {
            "-l" => links.push(rest.to_owned()),
            _ => paths.push(PathBuf::from(rest)),
        }
    }
    Ok((paths, links))
}

pub fn parse(input: &[u8], cx: &Context<'_>) -> Result<Output, Error> {
    let package = cx.package;
    let mut out = Output::default();
    let (Some(generated), Some(out_dir_text)) =
        (cx.out_dir_when_generated.to_str(), cx.out_dir.to_str())
    else {
        return Err(Error("an `OUT_DIR` that is not UTF-8".to_owned()));
    };
    let moved = !generated.is_empty() && generated != out_dir_text;
    let mut new_syntax_msrv: Option<Option<String>> = None;
    let mut rest = Some(input);
    while let Some(bytes) = rest {
        let line = match memchr::memchr(b'\n', bytes) {
            Some(at) => {
                rest = Some(&bytes[at + 1..]);
                &bytes[..at]
            }
            None => {
                rest = None;
                bytes
            }
        };
        let Some(line) = directive(line) else { continue };
        let old_syntax;
        let (key, value) = if let Some(data) = line.strip_prefix("cargo::") {
            if new_syntax_msrv.is_none() {
                new_syntax_msrv = Some(older_than_new_syntax(package)?);
            }
            if let Some(Some(msrv)) = &new_syntax_msrv {
                return Err(new_syntax_refused(Descr(package), msrv, data));
            }
            old_syntax = false;
            key_value(package, line, data, "cargo::")?
        } else if let Some(data) = line.strip_prefix("cargo:") {
            old_syntax = true;
            match data.split_once('=') {
                Some((key, value)) if reserved(key) => (key, value.trim_end()),
                _ => ("metadata", data),
            }
        } else {
            continue;
        };
        let value = match moved && value.contains(generated) {
            true => value.replace(generated, out_dir_text),
            false => value.to_owned(),
        };
        let (descr, whence) = (Descr(package), Whence(package));
        let syntax = if old_syntax { "cargo:" } else { "cargo::" };
        let missing_target = |kind: &str| {
            Error(format!(
                "invalid instruction `{syntax}{key}` from {whence}\n\
                 The package {descr} does not have a {kind} target."
            ))
        };
        let targets = &package.targets;
        match key {
            "rustc-flags" => {
                let (paths, links) = parse_rustc_flags(&value, &whence)?;
                out.library_links.extend(links);
                out.library_paths
                    .extend(paths.into_iter().map(|it| LibraryPath::new(it, cx.out_dir)));
            }
            "rustc-link-lib" => out.library_links.push(value),
            "rustc-link-search" => {
                out.library_paths.push(LibraryPath::new(PathBuf::from(value), cx.out_dir))
            }
            "rustc-link-arg-cdylib" | "rustc-cdylib-link-arg" => {
                if !targets.iter().any(|it| it.is_cdylib()) {
                    out.log_messages.push((
                        Severity::Warning,
                        format!(
                            "{syntax}{key} was specified in the build script of {descr}, \
                             but that package does not contain a cdylib target\n\
                             \n\
                             Allowing this was an unintended change in the 1.50 \
                             release, and may become an error in the future. \
                             For more information, see \
                             <https://github.com/rust-lang/cargo/issues/9562>."
                        ),
                    ));
                }
                out.linker_args.push((LinkArgTarget::Cdylib, value));
            }
            "rustc-link-arg-bins" => {
                if !targets.iter().any(|it| it.is_bin()) {
                    return Err(missing_target("bin"));
                }
                out.linker_args.push((LinkArgTarget::Bin, value));
            }
            "rustc-link-arg-bin" => {
                let Some((bin, arg)) = value.split_once('=') else {
                    return Err(Error(format!(
                        "invalid instruction `{syntax}{key}={value}` from {whence}\n\
                         The instruction should have the form {syntax}{key}=BIN=ARG"
                    )));
                };
                if !targets.iter().any(|it| it.is_bin() && it.name == bin) {
                    return Err(Error(format!(
                        "invalid instruction `{syntax}{key}` from {whence}\n\
                         The package {descr} does not have a bin target with the name `{bin}`."
                    )));
                }
                out.linker_args.push((LinkArgTarget::SingleBin(bin.to_owned()), arg.to_owned()));
            }
            "rustc-link-arg-tests" => {
                if !targets.iter().any(|it| it.is_test()) {
                    return Err(missing_target("test"));
                }
                out.linker_args.push((LinkArgTarget::Test, value));
            }
            "rustc-link-arg-benches" => {
                if !targets.iter().any(|it| it.is_bench()) {
                    return Err(missing_target("benchmark"));
                }
                out.linker_args.push((LinkArgTarget::Bench, value));
            }
            "rustc-link-arg-examples" => {
                if !targets.iter().any(|it| it.is_example()) {
                    return Err(missing_target("example"));
                }
                out.linker_args.push((LinkArgTarget::Example, value));
            }
            "rustc-link-arg" => out.linker_args.push((LinkArgTarget::All, value)),
            "rustc-cfg" => out.cfgs.push(value),
            "rustc-check-cfg" => out.check_cfgs.push(value),
            "rustc-env" => {
                let (name, val) = parse_rustc_env(&value, &whence)?;
                if name != "RUSTC_BOOTSTRAP" {
                    out.env.push((name, val));
                    continue;
                }
                let library = package.library_crate_name();
                let allowed_by_env = library.as_deref().is_some_and(|name| {
                    cx.rustc_bootstrap
                        .as_deref()
                        .is_some_and(|it| it.split(',').any(|it| it == name))
                });
                let message = format!(
                    "cannot set `RUSTC_BOOTSTRAP={val}` from {whence}.\n\
                     note: crates cannot set `RUSTC_BOOTSTRAP` themselves, as doing so would \
                     subvert the stability guarantees of Rust for your project."
                );
                if cx.nightly_features_allowed || allowed_by_env {
                    out.log_messages.push((Severity::Warning, message));
                } else {
                    let name = library.as_deref().unwrap_or("1");
                    return Err(Error(format!(
                        "{message}\n\
                         help: If you're sure you want to do this in your project, set the \
                         environment variable `RUSTC_BOOTSTRAP={name}` before running cargo \
                         instead."
                    )));
                }
            }
            "error" => out.log_messages.push((Severity::Error, value)),
            "warning" => out.log_messages.push((Severity::Warning, value)),
            "rerun-if-changed" => out.rerun_if_changed.push(PathBuf::from(value)),
            "rerun-if-env-changed" => out.rerun_if_env_changed.push(value),
            "metadata" => {
                let syntax = if old_syntax { "cargo:" } else { "cargo::metadata=" };
                let (key, value) = key_value(package, line, &value, syntax)?;
                out.metadata.push((key.to_owned(), value.to_owned()));
            }
            _ => {
                return Err(Error(format!(
                    "invalid output in {whence}: `{line}`\n\
                     Unknown key: `{key}`.\n\
                     {DOCS}"
                )));
            }
        }
    }
    Ok(out)
}

/// `rustc-env=NAME=VALUE`.
fn parse_rustc_env(
    value: &str,
    whence: impl std::fmt::Display,
) -> Result<(String, String), Error> {
    match value.split_once('=') {
        Some((name, value)) => Ok((name.to_owned(), value.to_owned())),
        None => Err(Error(format!("Variable rustc-env has no value in {whence}: {value}"))),
    }
}
