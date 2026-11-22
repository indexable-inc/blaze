//! `cargo metadata` read as one question (metadata_ask.rs): the exact line turso's
//! `hegeltest-c 0.30.1` script spawned through cbindgen 0.29.4 (c1814 check.out:21), the same
//! with cbindgen's `only_target_dependencies` platform, and the lines refused by name.

use crate::metadata_ask::{FIELDS, FeatureAsk, MetadataAsk, Reader, Refused, Why, classify, is_cargo_metadata};

/// c1814 check.out:21: `Spawn(NoSandbox { program: "cargo", args: [..] })`.
const HEGELTEST_C: &[&str] = &["metadata", "--all-features", "--format-version", "1", "--manifest-path", "/package/Cargo.toml"];

/// THE REAL LINE IS ONE QUESTION: hegeltest-c's spawn reads as every feature on, every platform,
/// format 1, the package's manifest. TWIN: cbindgen with `only_target_dependencies` adds
/// `--filter-platform <TARGET>` (cargo_metadata.rs `metadata`): the same ask but for the one
/// platform, and `--features a,b -F c` lists three. CONTROL: what is not `cargo metadata` and
/// each flag that could move the answer unread is refused at its index: `--config`, `-Z`, a
/// format other than 1, a second `--manifest-path`, a positional word, a value missing.
#[test]
fn metadata_ask_reads_cbindgens_line_and_refuses_what_it_does_not_read() {
    let alloc = blaze_own::testing::heap().alloc();
    assert!(is_cargo_metadata("cargo", HEGELTEST_C) && is_cargo_metadata("/usr/bin/cargo", HEGELTEST_C));
    let ask = classify("cargo", HEGELTEST_C, alloc.clone()).expect("hegeltest-c's line reads");
    assert_eq!(ask.manifest_path, Some("/package/Cargo.toml"));
    assert_eq!((ask.features.all, ask.features.no_default, ask.features.listed.len()), (true, false, 0));
    assert!(!ask.no_deps && ask.platforms.is_empty() && ask.format_given, "every package, every platform, format 1: {ask:?}");

    let mut with_target: std::vec::Vec<&str> = HEGELTEST_C.to_vec();
    with_target.extend(["--filter-platform", "aarch64-apple-darwin", "--features", "a,b", "-Fc", "-q"]);
    let twin = classify("cargo", &with_target, alloc.clone()).expect("TWIN: the platform form reads");
    assert_eq!(twin.platforms.as_slice(), ["aarch64-apple-darwin"]);
    assert_eq!(twin.features.listed.as_slice(), ["a", "b", "c"]);
    assert_eq!(twin.manifest_path, ask.manifest_path, "TWIN: the same manifest");

    let refused = |args: &[&str]| classify("cargo", args, alloc.clone()).map(|_| ()).unwrap_err();
    assert_eq!(classify("cargo", &["build"], alloc.clone()).map(|_| ()), Err(Refused { at: 0, why: Why::NotMetadata }), "CONTROL: not `cargo metadata`");
    assert_eq!(refused(&["metadata", "--config", "x=1"]), Refused { at: 1, why: Why::UnknownFlag }, "CONTROL: `--config` can move the answer");
    assert_eq!(refused(&["metadata", "-Zunstable-options"]), Refused { at: 1, why: Why::UnknownFlag });
    assert_eq!(refused(&["metadata", "--format-version", "2"]), Refused { at: 1, why: Why::FormatVersion });
    assert_eq!(refused(&["metadata", "--manifest-path", "a", "--manifest-path=b"]), Refused { at: 3, why: Why::Repeated });
    assert_eq!(refused(&["metadata", "extra"]), Refused { at: 1, why: Why::Positional });
    assert_eq!(refused(&["metadata", "--manifest-path"]), Refused { at: 1, why: Why::NoValue });
    assert!(format!("{}", refused(&["metadata", "--config", "x=1"])).contains("argument 1"), "a refusal names its argument");

    // No `--format-version`: read, and cargo's warning is the answer's to write.
    let bare = classify("cargo", &["metadata"], alloc).expect("a bare line reads");
    let MetadataAsk { manifest_path, features: FeatureAsk { all, no_default, listed }, no_deps, platforms, format_given } = bare;
    assert_eq!((manifest_path, all, no_default, listed.len(), no_deps, platforms.len(), format_given), (None, false, false, 0, false, 0, false));
}

/// THE FIELDS CBINDGEN CANNOT PARSE WITHOUT are each listed as required and as cbindgen's
/// (cargo_metadata.rs: `Metadata { packages, version, workspace_root }`, `Package { id,
/// dependencies, targets, features, manifest_path }` and its flattened `name`, `Dependency {
/// name, req, optional, uses_default_features, features }`, `Target { name, kind, src_path }`;
/// the `Option` and `#[serde(default)]` ones are not required). CONTROL: a field no reader
/// requires (`resolve`, which cbindgen's fork removed) is listed as not required.
#[test]
fn metadata_ask_lists_every_field_cbindgen_requires() {
    const REQUIRED: &[&str] = &[
        "version",
        "workspace_root",
        "packages[]",
        "packages[].name",
        "packages[].id",
        "packages[].manifest_path",
        "packages[].features",
        "packages[].dependencies[]",
        "packages[].dependencies[].name",
        "packages[].dependencies[].req",
        "packages[].dependencies[].optional",
        "packages[].dependencies[].uses_default_features",
        "packages[].dependencies[].features",
        "packages[].targets[]",
        "packages[].targets[].name",
        "packages[].targets[].kind",
        "packages[].targets[].src_path",
    ];
    for path in REQUIRED {
        let field = FIELDS.iter().find(|field| field.path == *path).unwrap_or_else(|| panic!("`{path}` is listed"));
        assert!(field.required && field.reader == Reader::Cbindgen, "`{path}` is cbindgen's, required: {field:?}");
    }
    let mut required: std::vec::Vec<&str> = FIELDS.iter().filter(|field| field.required).map(|field| field.path).collect();
    required.sort_unstable();
    let mut expected = REQUIRED.to_vec();
    expected.sort_unstable();
    assert_eq!(required, expected, "no other field is marked required");
    let resolve = FIELDS.iter().find(|field| field.path == "resolve").expect("`resolve` is listed");
    assert!(!resolve.required, "CONTROL: `resolve` is no reader's requirement");
}
