//! One test per profile lint (`profile_hot`, `stale_hot_marker`, `hot_marked_cold`), each
//! against an indexed profile made in the test ([`crate::profile::indexed::written`]) that
//! names the crate `main`'s functions by their legacy symbols, and one that with no profile
//! configured they say nothing.

use std::{path::PathBuf, sync::Arc};

use super::tests::{ALLOC_STD, MINICORE};
use crate::{
    conf::Conf,
    profile::{self, COLD_DEFAULT_PPB, HOT_DEFAULT_PPB, Loaded, Settings, indexed},
    tests::lint_with,
};

const MAIN: &str = "//- /src/lib.rs crate:main deps:std\n";

/// The legacy symbol of `main::<name>`, as `rustc -C symbol-mangling-version=legacy` writes it.
fn symbol(name: &str) -> String {
    format!("_ZN4main{}{name}17h0123456789abcdefE", name.len())
}

/// A configuration naming a profile of `counts`: each function of `main` by name, with the
/// sum of its counters.
fn conf(counts: &[(&str, u64)]) -> Conf {
    let symbols: Vec<String> = counts.iter().map(|(name, _)| symbol(name)).collect();
    let counters: Vec<[u64; 1]> = counts.iter().map(|&(_, count)| [count]).collect();
    let records: Vec<(&str, u64, &[u64])> = symbols
        .iter()
        .zip(&counters)
        .map(|(name, counts)| (name.as_str(), 1, counts.as_slice()))
        .collect();
    let bytes = indexed::written(indexed::IR_PROF, &records);
    let settings = Settings {
        path: PathBuf::from("/profiles/app.profdata"),
        named_in: "`clippy.toml`".to_owned(),
        hot_ppb: HOT_DEFAULT_PPB,
        cold_ppb: COLD_DEFAULT_PPB,
    };
    let loaded =
        Loaded { settings, profile: Arc::new(profile::read(&bytes).expect("the profile reads")) };
    Conf { profile: Some(loaded), ..Conf::default() }
}

fn fixture(main: &str) -> String {
    format!("{MINICORE}{ALLOC_STD}{MAIN}{main}")
}

#[test]
fn profile_hot_lints_a_hot_function_as_hot_code() {
    let main = fixture(
        r#"
pub fn busy(s: &String) {
    let _ = s.clone();
}
pub fn idle(s: &String) {
    let _ = s.clone();
}
"#,
    );
    let flags = "-A clippy::all --warn=blaze::profile_hot";
    let out = lint_with(&main, flags, conf(&[("busy", 991), ("idle", 9)]));
    assert_eq!(out.matches("warning: this `clone` of").count(), 1, "{out}");
    assert!(out.contains("src/lib.rs:3:"), "the clone in `busy`, not in `idle`:\n{out}");
    assert!(out.contains("implied by `blaze::profile_hot`: this function is hot per profile `app.profdata`, 99% of its counts"), "{out}");
    // Allowed, `profile_hot` raises nothing.
    let out = lint_with(
        &main,
        "-A clippy::all --allow=blaze::profile_hot",
        conf(&[("busy", 991), ("idle", 9)]),
    );
    assert!(!out.contains("clone"), "{out}");
}

#[test]
fn stale_hot_marker_is_a_hot_marked_function_the_profile_never_ran() {
    let main = fixture(
        r#"
#[deny(blaze::hot)]
pub fn idle() {}
#[deny(blaze::hot)]
pub fn busy() {}
"#,
    );
    let out = lint_with(
        &main,
        "-A clippy::all --warn=blaze::stale_hot_marker",
        conf(&[("busy", 100), ("idle", 0)]),
    );
    assert_eq!(out.matches("warning: `fn").count(), 1, "{out}");
    assert!(
        out.contains("`fn idle` is marked hot (`#[deny(blaze::hot)]`), and profile `app.profdata` counts none of its code"),
        "{out}"
    );
}

#[test]
fn hot_marked_cold_is_a_cold_function_the_profile_shows_hot() {
    let main = fixture(
        r#"
#[cold]
pub fn busy() {}
#[cold]
pub fn rare() {}
"#,
    );
    let out = lint_with(
        &main,
        "-A clippy::all --warn=blaze::hot_marked_cold",
        conf(&[("busy", 5_000), ("rare", 1)]),
    );
    assert_eq!(out.matches("warning: `fn").count(), 1, "{out}");
    assert!(
        out.contains(
            "`fn busy` is `#[cold]`, and profile `app.profdata` counts 100% of its counts"
        ),
        "{out}"
    );
}

#[test]
fn without_a_profile_the_profile_lints_say_nothing() {
    let main = fixture(
        r#"
#[deny(blaze::hot)]
pub fn idle() {}
#[cold]
pub fn busy(s: &String) {
    let _ = s.clone();
}
"#,
    );
    let flags = "-A clippy::all --warn=blaze::profile_hot --warn=blaze::stale_hot_marker --warn=blaze::hot_marked_cold";
    assert_eq!(lint_with(&main, flags, Conf::default()), "");
}

#[test]
fn a_configured_profile_that_does_not_read_is_an_error_naming_it() {
    let dir = std::env::temp_dir().join(format!("blaze-lints-profile-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("clippy.toml");
    std::fs::write(&path, "profile = \"missing.profdata\"\nprofile-hot-percent = 5\n").unwrap();
    let error = Conf::load(Some(path.clone()), None).unwrap().with_profile(None).unwrap_err();
    assert!(error.contains(&dir.join("missing.profdata").display().to_string()), "{error}");
    std::fs::write(dir.join("raw.profdata"), b"not indexed").unwrap();
    std::fs::write(&path, "profile = \"raw.profdata\"\n").unwrap();
    let error = Conf::load(Some(path.clone()), None).unwrap().with_profile(None).unwrap_err();
    assert!(error.contains("is not an indexed profile"), "{error}");
    for (text, says) in [
        ("profile = 3\n", "expected a path for `profile`"),
        (
            "profile = \"a\"\nprofile-hot-percent = \"hot\"\n",
            "expected a percent for `profile-hot-percent`",
        ),
        ("profile = \"a\"\nprofile-cold-percent = 120\n", "from 0 to 100"),
    ] {
        std::fs::write(&path, text).unwrap();
        let error = Conf::load(Some(path.clone()), None).expect_err(text);
        assert!(error.contains(says), "{text}: {error}");
    }
    // No profile named: nothing to read, nothing on.
    std::fs::write(&path, "msrv = \"1.70\"\n").unwrap();
    assert!(Conf::load(Some(path), None).unwrap().with_profile(None).unwrap().profile.is_none());
    std::fs::remove_dir_all(&dir).unwrap();
}
