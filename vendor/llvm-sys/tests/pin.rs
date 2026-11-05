// blaze: llvm-sys's `pin` test target, pin.rs (which build.rs includes) with its tests. They are
// here and not in pin.rs so that blaze-codegen-code, which includes pin.rs, does not compile
// them into its own test binary: `checks_a_prefix_on_disk` writes and runs an `llvm-config`
// script, as blaze-codegen-code's `a_prefix_is_checked_to_be_the_pin` does, and two such tests
// on parallel threads of one binary race to ETXTBSY.

include!("../pin.rs");

mod tests {
    use super::*;

    const LLVM_JSON: &str = include_str!("../../../crates/hir-ty-codegen/llvm.json");

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
    fn reads_a_targets_sha256() {
        let sha = pin_sha256(LLVM_JSON, "x86_64-unknown-linux-gnu").unwrap().expect("a release");
        assert_eq!(sha.len(), 64, "{sha}");
        assert_ne!(Some(sha), pin_sha256(LLVM_JSON, "aarch64-apple-darwin").unwrap());
        assert_eq!(pin_sha256(LLVM_JSON, "riscv64gc-unknown-linux-gnu"), Ok(None));
        let unsummed = r#"{"targets": {"t": {"name": "n"}, "u": {"sha256": "ab"}}}"#;
        assert!(pin_sha256(unsummed, "t").is_err());
        assert_eq!(pin_sha256(unsummed, "u").as_deref(), Ok(Some("ab")));
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
            "[workspace.metadata.blaze.native.llvm]",
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

    /// One test for every prefix on disk, run in order, and the only test in this binary that
    /// writes or runs a program: a script written and run while another test thread forks can
    /// fail with ETXTBSY (rust-lang/rust#114554). blaze-codegen-code includes pin.rs without
    /// this file, so its own script test does not share a binary with this one.
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
