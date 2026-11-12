//! `BLAZE_VERSION` and `BLAZE_VERSION_LINE`: which blaze this is (see [`version`]);
//! `BLAZE_CODEGEN_CODE`, the code identity of its code generator, which stored templates are
//! keyed by; `BLAZE_FRONT_END_CODE`, that of its front end, which images, stored goals and
//! bodies and kept expansions are keyed by; and `BLAZE_RECORD_CODE`, that of what makes a unit's
//! record, which records and the standard library's shared directory are keyed by with the other
//! two. All three are worked out in one walk of the workspace (`blaze_codegen_code::emit_all`).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build_version.rs");
    seed();
    version();
    blaze_codegen_code::emit_all(&[
        &blaze_codegen_code::CODEGEN,
        &blaze_codegen_code::FRONT_END,
        &blaze_codegen_code::RECORD,
    ]);
}

/// The `seed` feature is the seed profile, `--no-default-features --features seed`
/// (docs/future/bootstrap.md): no C, C++ or LLVM. With a feature that brings them back (`default`
/// among them, which is on unless `--no-default-features` is given) it is refused, naming both,
/// never built as a seed that isn't one.
fn seed() {
    let on = |feature: &str| {
        std::env::var_os(format!("CARGO_FEATURE_{}", feature.to_uppercase().replace('-', "_")))
            .is_some()
    };
    if !on("seed") {
        return;
    }
    let not_in_seed = ["default", "native"];
    let also: Vec<&str> = not_in_seed.into_iter().filter(|it| on(it)).collect();
    if !also.is_empty() {
        panic!(
            "blaze-engine's `seed` feature is the seed profile, which has no C, C++ or LLVM, and \
             `{}` {} on too: build the seed with `--no-default-features --features seed` and no \
             other feature",
            also.join("`, `"),
            if also.len() == 1 { "is" } else { "are" }
        );
    }
}

// `version`: which blaze this is (`BLAZE_VERSION`, `BLAZE_VERSION_LINE`). Its own file, which
// blaze-thin's build script includes too, so the thin `blaze` and this one are told the same.
include!("build_version.rs");
