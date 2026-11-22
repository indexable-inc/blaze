//! cargo's rustflags rule (rustflags.rs), on turso's own config: its `[build]` and
//! `[target.'cfg(target_vendor = "apple")']` tables both carry `--cfg=tokio_unstable`, and tokio
//! exports `runtime::RngSeed` only under that cfg.

use super::{CfgSpecError, Chosen, FlagsFrom, RustflagsConfig, RustflagsEnv, RustflagsError, Unit, cfg_spec, choose, target_variable};

const TRIPLE: &str = "aarch64-apple-darwin";
/// Part of what the pinned toolchain states for [`TRIPLE`].
const APPLE: [(&str, Option<&str>); 4] = [("unix", None), ("target_os", Some("macos")), ("target_vendor", Some("apple")), ("debug_assertions", None)];
const LINUX: [(&str, Option<&str>); 3] = [("unix", None), ("target_os", Some("linux")), ("target_env", Some("gnu"))];
/// No `--target` (blaze takes none): every unit gets the flags.
const LIB: Unit = Unit { target_given: false, host_unit: false };
const SCRIPT: Unit = Unit { target_given: false, host_unit: true };

fn chosen(files: &[&str], env: RustflagsEnv<'_>, atoms: &[(&str, Option<&str>)], unit: Unit) -> Result<(FlagsFrom, Vec<String>, Vec<String>, bool), String> {
    let alloc = blaze_own::testing::heap().alloc();
    let mut config = RustflagsConfig::new(alloc);
    for (at, text) in files.iter().enumerate() {
        config.read(if at == 0 { ".cargo/config.toml" } else { ".cargo/more.toml" }, text).map_err(|e| e.to_string())?;
    }
    let Chosen { from, cfg, not_applied, settled } = choose(&config, env, TRIPLE, atoms, unit, alloc).map_err(|e| e.to_string())?;
    let cfg = cfg.iter().map(|(name, value)| match value {
        Some(value) => format!("{}=\"{}\"", name.as_str(), value.as_str()),
        None => name.as_str().to_owned(),
    });
    Ok((from, cfg.collect(), not_applied.iter().map(|flag| flag.as_str().to_owned()).collect(), settled))
}

const BUILD: &str = "[build]\nrustflags = [\"--cfg=tokio_unstable\"]\n";

/// ORACLE: `[build] rustflags` reaches the unit's cfg. CONTROL (rule 23): the same file with
/// `--cfg=other` gives `other` and not `tokio_unstable`, so the atom is the flag's. TWIN, no
/// config: nothing.
#[test]
fn a_build_rustflags_cfg_is_a_unit_cfg() {
    assert_eq!(chosen(&[BUILD], RustflagsEnv::default(), &APPLE, LIB), Ok((FlagsFrom::Build, vec!["tokio_unstable".into()], vec![], true)));
    let other = "[build]\nrustflags = [\"--cfg=other\"]\n";
    assert_eq!(chosen(&[other], RustflagsEnv::default(), &APPLE, LIB).map(|c| c.1), Ok(vec!["other".into()]));
    assert_eq!(chosen(&[], RustflagsEnv::default(), &APPLE, LIB), Ok((FlagsFrom::Unset, vec![], vec![], true)));
}

/// The sources are mutually exclusive: a matching `target.'cfg(..)'` table with no `--cfg` is
/// the one source, and `[build]` is not read (TWIN: on Linux the apple table does not match, and
/// `[build]` is the source). Its other flags are named, not applied.
#[test]
fn a_matching_target_table_shadows_build() {
    let both = "[build]\nrustflags = [\"--cfg=tokio_unstable\"]\n[target.'cfg(target_vendor = \"apple\")']\nrustflags = [\"-C\", \"link-args=-Wl,-undefined,dynamic_lookup\"]\n";
    assert_eq!(
        chosen(&[both], RustflagsEnv::default(), &APPLE, LIB),
        Ok((FlagsFrom::Target, vec![], vec!["-C".into(), "link-args=-Wl,-undefined,dynamic_lookup".into()], true))
    );
    assert_eq!(chosen(&[both], RustflagsEnv::default(), &LINUX, LIB).map(|c| (c.0, c.1)), Ok((FlagsFrom::Build, vec!["tokio_unstable".into()])));
}

/// The environment comes first: `CARGO_ENCODED_RUSTFLAGS` over `RUSTFLAGS` over every file,
/// and `--cfg X` (two flags) reads as `--cfg=X`.
#[test]
fn the_environment_is_the_first_source() {
    let plain = RustflagsEnv { plain: Some(" --cfg tokio_unstable "), ..RustflagsEnv::default() };
    assert_eq!(chosen(&[], plain, &APPLE, LIB), Ok((FlagsFrom::Env, vec!["tokio_unstable".into()], vec![], true)));
    assert_eq!(chosen(&[BUILD], RustflagsEnv { plain: Some("--cfg=a"), ..RustflagsEnv::default() }, &APPLE, LIB).map(|c| c.1), Ok(vec!["a".into()]));
    let encoded = RustflagsEnv { encoded: Some("--cfg\x1ffeature=\"x\""), plain: Some("--cfg=a"), ..RustflagsEnv::default() };
    assert_eq!(chosen(&[BUILD], encoded, &APPLE, LIB), Ok((FlagsFrom::EncodedEnv, vec!["feature=\"x\"".into()], vec![], true)));
    // An empty encoded value is no flags, and still the one source.
    assert_eq!(chosen(&[BUILD], RustflagsEnv { encoded: Some(""), ..RustflagsEnv::default() }, &APPLE, LIB), Ok((FlagsFrom::EncodedEnv, vec![], vec![], true)));
}

/// Without `--target` a build script gets the flags too; with `--target` it gets none (the
/// TWIN), while a target unit of that build still does (the CONTROL).
#[test]
fn with_a_target_given_a_host_unit_gets_no_flags() {
    assert_eq!(chosen(&[BUILD], RustflagsEnv::default(), &APPLE, SCRIPT).map(|c| c.1), Ok(vec!["tokio_unstable".into()]));
    let script_of_target = Unit { target_given: true, host_unit: true };
    assert_eq!(chosen(&[BUILD], RustflagsEnv::default(), &APPLE, script_of_target), Ok((FlagsFrom::HostOfTarget, vec![], vec![], true)));
    let lib_of_target = Unit { target_given: true, host_unit: false };
    assert_eq!(chosen(&[BUILD], RustflagsEnv::default(), &APPLE, lib_of_target).map(|c| c.1), Ok(vec!["tokio_unstable".into()]));
}

/// cargo's second turn: a `cfg(..)` table matched by a `--cfg` the first turn's flags set
/// (rustc prints it in `--print cfg`) joins on the second, after the triple's table.
#[test]
fn a_cfg_table_matched_by_a_flag_joins_on_the_second_turn() {
    let text = "[target.aarch64-apple-darwin]\nrustflags = [\"--cfg=tokio_unstable\"]\n[target.'cfg(tokio_unstable)']\nrustflags = [\"--cfg=second\"]\n";
    assert_eq!(chosen(&[text], RustflagsEnv::default(), &APPLE, LIB), Ok((FlagsFrom::Target, vec!["tokio_unstable".into(), "second".into()], vec![], true)));
}

/// An included file is read first, the including one after: arrays join in that order, a
/// string replaces, a string over an array is refused by key.
#[test]
fn config_files_merge_as_cargo_merges_them() {
    let more = "[build]\nrustflags = [\"--cfg=b\"]\n";
    assert_eq!(chosen(&["[build]\nrustflags = [\"--cfg=a\"]\n", more], RustflagsEnv::default(), &APPLE, LIB).map(|c| c.1), Ok(vec!["a".into(), "b".into()]));
    assert_eq!(chosen(&["[build]\nrustflags = \"--cfg a\"\n", "[build]\nrustflags = \"--cfg b -C opt-level=1\"\n"], RustflagsEnv::default(), &APPLE, LIB).map(|c| (c.1, c.2)), Ok((vec!["b".into()], vec!["-C".into(), "opt-level=1".into()])));
    let mixed = chosen(&["[build]\nrustflags = \"--cfg a\"\n", more], RustflagsEnv::default(), &APPLE, LIB).expect_err("a string under an array");
    assert!(mixed.contains("build.rustflags"), "{mixed}");
    let alloc = blaze_own::testing::heap().alloc();
    let tempo = "include = [{ path = \"orb.toml\", optional = true }]\n[alias]\nx = \"run\"\n";
    let includes = RustflagsConfig::includes(".cargo/config.toml", tempo, alloc).expect("tempo's include");
    assert_eq!(includes.iter().map(|i| (i.path.as_str(), i.optional)).collect::<Vec<_>>(), [("orb.toml", true)]);
}

/// The one `--cfg` parser reads `name` and `name="value"` by the cfg grammar; a predicate, two
/// names or a lone `--cfg` is refused naming the flags' source.
#[test]
fn a_cfg_spec_is_one_atom() {
    let alloc = blaze_own::testing::heap().alloc();
    let (name, value) = cfg_spec("feature = \"x y\"", alloc).expect("a pair");
    assert_eq!((name.as_str(), value.as_ref().map(|v| v.as_str())), ("feature", Some("x y")));
    assert!(matches!(cfg_spec("all(a)", alloc), Err(CfgSpecError::NotOneAtom)));
    assert!(matches!(cfg_spec("a b", alloc), Err(CfgSpecError::Parse(_))));
    let dangling = chosen(&["[build]\nrustflags = [\"--cfg\"]\n"], RustflagsEnv::default(), &APPLE, LIB).expect_err("a lone --cfg");
    assert!(dangling.contains("build.rustflags") && dangling.contains("no spec"), "{dangling}");
    let config = RustflagsConfig::new(alloc);
    let merged = RustflagsEnv { build: Some("--cfg x"), ..RustflagsEnv::default() };
    assert!(matches!(choose(&config, merged, TRIPLE, &APPLE, LIB, alloc), Err(RustflagsError::NotMerged { var: "CARGO_BUILD_RUSTFLAGS" })));
    assert_eq!(target_variable("aarch64-apple-darwin", alloc).as_str(), "CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS");
}
