//! flecs core (FLECS_CUSTOM_BUILD plus FLECS_LOG), always -O2, plus the addons whose features are on: `system`, `pipeline`,
//! `meta`, `script`, `json` (never REST, HTTP or alerts). den-core and den-web build the core only; den-native turns on the
//! first four. `json` is flecs's own serializer of type info, query results and values (den-flecs's `reflect`, the repl's
//! live types: docs/repl-live-types.md).
//!
//! wasm32-unknown-unknown has no C library: `wasm-libc/` is a declaration-only set of headers plus `libc.c` (strings, ctype,
//! printf, stub files). `ECS_TARGET_POSIX` makes flecs use the plain C calls (`strncpy`, `snprintf`) rather than the `_s` ones.
//! malloc, calloc, realloc, free and abort come from Rust (src/lib.rs). The core takes no pthread: a world lives on one thread.
//! Use an unwrapped clang for wasm (`CC_wasm32_unknown_unknown`, `AR_wasm32_unknown_unknown`): a nix cc-wrapper adds host flags.
use std::env;
use std::path::PathBuf;

fn main() {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let flecs = root.join("../flecs");
    let target = env::var("TARGET").expect("TARGET");
    let mut b = cc::Build::new();
    b.file(flecs.join("flecs.c"))
        .file(root.join("csrc/layout.c"))
        .include(&flecs)
        .define("FLECS_CUSTOM_BUILD", None)
        .define("FLECS_LOG", None)
        .opt_level(2)
        .debug(false)
        .warnings(false)
        .flag("-std=gnu99");
    // flecs's debug mode follows Rust's debug assertions. With them (tests, dev builds) flecs runs
    // FLECS_DEBUG: every argument check plus its debug-only bookkeeping (exclusive-access and
    // table-lock checks, the set_child_order membership scan). Without them (release) it runs
    // NDEBUG plus FLECS_KEEP_ASSERT: the bookkeeping goes but every ecs_check and ecs_assert stays.
    // den-ecs checks each argument flecs would refuse before calling it, so neither mode should
    // ever fire; if one does, that is a gap in those checks, and an abort is better than going on
    // past it into memory flecs no longer vouches for (plain NDEBUG would). Measured on dev-aws-1
    // (100k entities spawned under 50 parents with 3 components each, best of 5, release Rust):
    // FLECS_DEBUG 133 ms, NDEBUG + KEEP_ASSERT 89 ms, plain NDEBUG 57 ms.
    // FLECS_SOFT_ASSERT: ecs_check and ecs_throw (the argument and constraint checks) log and fail the call instead of aborting
    // the process; ecs_assert and ecs_always_assert (internal sanity checks) still abort. See docs/upstream/flecs-soft-errors/.
    b.define("FLECS_SOFT_ASSERT", None);
    if env::var_os("CARGO_CFG_DEBUG_ASSERTIONS").is_none() {
        b.define("NDEBUG", None).define("FLECS_KEEP_ASSERT", None);
    }
    // Addons, one cargo feature each (script pulls in meta, doc and parser itself; json pulls in meta and doc; pipeline pulls in system).
    for (feature, addon) in [("SYSTEM", "FLECS_SYSTEM"), ("PIPELINE", "FLECS_PIPELINE"), ("META", "FLECS_META"), ("SCRIPT", "FLECS_SCRIPT"), ("JSON", "FLECS_JSON")] {
        if env::var_os(format!("CARGO_FEATURE_{feature}")).is_some() {
            b.define(addon, None);
        }
    }
    // The pipeline addon brings flecs's own OS API implementation (pthread threads and locks), which wasm32 has no C library for.
    assert!(
        !(target.starts_with("wasm32") && env::var_os("CARGO_FEATURE_PIPELINE").is_some()),
        "hyperion-flecs-c: the pipeline addon needs threads; build wasm32 with the core only"
    );
    if target.starts_with("wasm32") {
        let libc = root.join("wasm-libc");
        b.define("ECS_TARGET_POSIX", None)
            .flag("-ffreestanding")
            .flag("-nostdlibinc")
            .include(libc.join("include"))
            .file(libc.join("libc.c"));
    } else {
        b.flag_if_supported("-fno-sanitize=undefined");
    }
    b.compile("denflecs");
    for p in ["../flecs/flecs.c", "../flecs/flecs.h", "csrc", "wasm-libc"] {
        println!("cargo:rerun-if-changed={}", root.join(p).display());
    }
    println!("cargo:rerun-if-env-changed=CC_wasm32_unknown_unknown");
}
