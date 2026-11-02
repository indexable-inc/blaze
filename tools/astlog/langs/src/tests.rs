use std::path::PathBuf;

use super::*;

mod validation;

#[test]
fn test_detect_rust() {
    let path = PathBuf::from("src/main.rs");
    assert_eq!(detect(&path), Some(Lang::Rust));
}

#[test]
fn test_detect_python() {
    assert_eq!(detect(&PathBuf::from("script.py")), Some(Lang::Python));
    assert_eq!(detect(&PathBuf::from("types.pyi")), Some(Lang::Python));
}

#[test]
fn test_detect_toml() {
    assert_eq!(detect(&PathBuf::from("Cargo.toml")), Some(Lang::Toml));
    assert_eq!(detect(&PathBuf::from("clippy.toml")), Some(Lang::Toml));
}

#[test]
fn test_detect_unknown() {
    assert_eq!(detect(&PathBuf::from("file.xyz")), None);
    assert_eq!(detect(&PathBuf::from("flake.nix")), None);
    assert_eq!(detect(&PathBuf::from("BUCK")), None);
}

#[test]
fn test_detect_from_extension() {
    assert_eq!(detect_from_extension("rs"), Some(Lang::Rust));
    assert_eq!(detect_from_extension(".rs"), Some(Lang::Rust));
    assert_eq!(detect_from_extension("toml"), Some(Lang::Toml));
    assert_eq!(detect_from_extension("py"), Some(Lang::Python));
    assert_eq!(detect_from_extension("unknown"), None);
}
