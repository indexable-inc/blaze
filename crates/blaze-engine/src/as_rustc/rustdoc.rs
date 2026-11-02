//! blaze as the `RUSTDOC` a build script is given: it says which rustdoc it is, as the rustdoc
//! of blaze's release would, and documents nothing. Anything else a script asks of it fails with
//! an error that names the command, as no rustdoc runs (docs/no-rustc.md).

use std::{ffi::OsString, io::Write as _, path::Path};

use blaze_units::stdlib::target;

const NAME: &str = "rustdoc";

/// Whether this process was started as `rustdoc`, which is to be this.
pub(crate) fn is_me() -> bool {
    let Some(first) = std::env::args_os().next() else { return false };
    Path::new(&first).file_stem().is_some_and(|it| it == NAME)
}

/// Answers what the script asked rustdoc with the arguments of this process, and ends as
/// rustdoc would have.
pub(crate) fn command() -> ! {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let host = blaze_cargo::toolchain::host();
    let code = match answer(&args, host) {
        Ok(said) => match std::io::stdout().write_all(said.as_bytes()).and_then(|()| std::io::stdout().flush()) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("error: writing rustdoc's answer: {error}");
                1
            }
        },
        Err(why) => {
            let shown: Vec<_> = args.iter().map(|it| it.to_string_lossy()).collect();
            eprintln!("error: blaze can't answer `rustdoc {}` for a build script: {why}", shown.join(" "));
            1
        }
    };
    std::process::exit(code)
}

/// What rustdoc prints to `args` on `host`: its version, plain or verbose.
fn answer(args: &[OsString], host: Option<&str>) -> Result<String, String> {
    let mut verbose = false;
    let mut version = false;
    for arg in args {
        match arg.to_str() {
            Some("-V" | "--version") => version = true,
            Some("-v" | "--verbose") => verbose = true,
            Some("-vV" | "-Vv") => (version, verbose) = (true, true),
            _ => return Err(UNANSWERED.to_owned()),
        }
    }
    if !version {
        return Err(UNANSWERED.to_owned());
    }
    if !verbose {
        return Ok(format!("{NAME} {}\n", target::rustc_version()));
    }
    let host = host.ok_or("blaze knows no target triple of this machine")?;
    let told = target::rustc_verbose_version(host);
    let told = told.replacen("rustc ", "rustdoc ", 1).replacen("binary: rustc", "binary: rustdoc", 1);
    Ok(told)
}

const UNANSWERED: &str = "blaze documents nothing for a build script, and runs no rustdoc (docs/no-rustc.md)";

#[cfg(test)]
mod tests {
    use super::*;

    fn asked(args: &[&str]) -> Result<String, String> {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        answer(&args, Some("x86_64-unknown-linux-gnu"))
    }

    #[test]
    fn says_its_version() {
        let plain = asked(&["--version"]).unwrap();
        assert_eq!(plain, format!("rustdoc {}\n", target::rustc_version()));
        assert_eq!(asked(&["-V"]).unwrap(), plain);
        let verbose = asked(&["-vV"]).unwrap();
        assert!(verbose.starts_with(&plain), "{verbose}");
        assert!(verbose.contains("\nbinary: rustdoc\n"), "{verbose}");
        assert!(verbose.contains("\nhost: x86_64-unknown-linux-gnu\n"), "{verbose}");
        assert!(!verbose.contains("binary: rustc"), "{verbose}");
        assert_eq!(asked(&["--version", "--verbose"]).unwrap(), verbose);
    }

    #[test]
    fn documents_nothing() {
        for args in [&[][..], &["src/lib.rs"], &["-v"], &["--test", "src/lib.rs"], &["-V", "src/lib.rs"]] {
            assert_eq!(asked(args), Err(UNANSWERED.to_owned()), "{args:?}");
        }
    }
}
