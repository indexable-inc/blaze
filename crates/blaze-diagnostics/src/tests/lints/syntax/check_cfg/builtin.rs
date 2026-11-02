//! `explicit_builtin_cfgs_in_flags`: `tests/ui/cfg/disallowed-cli-cfgs.rs`'s revisions. The
//! lint has no span, so the test's source is only what makes it a crate.

use super::{flags, ui_cfg};

const SOURCE: &str = "fn main() {}\n";

fn refused(shown: &str, name: &str, flag: &str) -> String {
    format!(
        "error: unexpected `--cfg {shown}` flag
   |
   = note: config `{name}` is only supposed to be controlled by `{flag}`
   = note: manually setting a built-in cfg can and does create incoherent behaviors
   = note: `#[deny(explicit_builtin_cfgs_in_flags)]` on by default

error: aborting due to 1 previous error

"
    )
}

#[test]
fn a_cfg_rustc_sets_is_refused_on_the_command_line() {
    let cases = [
        ("overflow_checks", "overflow_checks", "overflow_checks", "-C overflow-checks"),
        ("debug_assertions", "debug_assertions", "debug_assertions", "-C debug-assertions"),
        ("ub_checks", "ub_checks", "ub_checks", "-Z ub-checks"),
        (r#"sanitize="cfi""#, r#"sanitize="cfi""#, "sanitize", "-Z sanitizer"),
        ("proc_macro", "proc_macro", "proc_macro", "--crate-type proc-macro"),
        (r#"panic="abort""#, r#"panic="abort""#, "panic", "-C panic"),
        (r#"target_feature="sse3""#, r#"target_feature="sse3""#, "target_feature", "-C target-feature"),
        ("unix", "unix", "unix", "--target"),
        ("windows", "windows", "windows", "--target"),
        (r#"target_os="linux""#, r#"target_os="linux""#, "target_os", "--target"),
        ("target_env", "target_env", "target_env", "--target"),
        ("target_vendor", "target_vendor", "target_vendor", "--target"),
        (r#"target_has_atomic="32""#, r#"target_has_atomic="32""#, "target_has_atomic", "--target"),
        ("target_thread_local", "target_thread_local", "target_thread_local", "--target"),
        (r#"relocation_model="a""#, r#"relocation_model="a""#, "relocation_model", "--target"),
        (r#"fmt_debug="shallow""#, r#"fmt_debug="shallow""#, "fmt_debug", "-Z fmt-debug"),
        ("target_has_reliable_f16", "target_has_reliable_f16", "target_has_reliable_f16", "--target"),
    ];
    for (cfg, shown, name, flag) in cases {
        let options = flags(&[], &[cfg], false);
        let stderr = ui_cfg("", "disallowed-cli-cfgs.rs", "edition:2015", SOURCE, &options);
        assert_eq!(stderr, refused(shown, name, flag), "--cfg {cfg}");
    }
}

#[test]
fn a_cfg_rustc_sets_another_way_is_not_refused() {
    for cfg in ["test", "unix=\"x\"", "target_os", r#"panic="other""#, r#"feature="std""#, "debug_assertions=\"x\""] {
        let options = flags(&[], &[cfg], false);
        assert_eq!(ui_cfg("", "disallowed-cli-cfgs.rs", "edition:2015", SOURCE, &options), "", "--cfg {cfg}");
    }
}

#[test]
fn a_cfg_given_twice_is_refused_once() {
    let options = flags(&[], &["unix", "unix"], false);
    let stderr = ui_cfg("", "disallowed-cli-cfgs.rs", "edition:2015", SOURCE, &options);
    assert_eq!(stderr, refused("unix", "unix", "--target"));
}
