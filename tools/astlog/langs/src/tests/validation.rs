use super::super::*;

#[test]
fn test_language_all() {
    assert_eq!(Lang::all(), &[Lang::Rust, Lang::Toml, Lang::Python]);
}

#[test]
fn test_language_name() {
    assert_eq!(Lang::Rust.name(), "Rust");
    assert_eq!(Lang::Toml.name(), "TOML");
    assert_eq!(Lang::Python.name(), "Python");
}

#[test]
fn test_tree_sitter_languages() {
    for lang in Lang::all() {
        assert!(lang.to_tree_sitter().node_kind_count() > 0, "{lang:?}");
    }
}

#[test]
fn test_rust_profile() {
    let profile = Lang::Rust.profile();
    assert!(profile.is_atomic("use_declaration"));
    assert!(!profile.is_atomic("function_item"));
    assert!(profile.is_commutative("declaration_list"));
    assert!(profile.is_comment("line_comment"));
}

#[test]
fn test_python_profile() {
    let profile = Lang::Python.profile();
    assert!(profile.is_atomic("import_statement"));
    assert!(profile.is_commutative("dictionary"));
}

#[test]
fn test_toml_profile() {
    let profile = Lang::Toml.profile();
    assert!(profile.is_commutative("table"));
    assert!(profile.is_comment("comment"));
}

#[test]
fn test_all_languages_have_valid_profiles() {
    for lang in Lang::all() {
        let profile = lang.profile();

        assert!(!profile.name.is_empty(), "Lang {lang:?} has empty name");

        assert!(
            !profile.extensions.is_empty() || !profile.file_names.is_empty(),
            "Lang {lang:?} has no extensions or file names"
        );
    }
}

#[test]
fn test_all_languages_can_create_parser() -> Result<(), tree_sitter::LanguageError> {
    for lang in Lang::all() {
        tree_sitter::Parser::new().set_language(&lang.to_tree_sitter())?;
    }
    Ok(())
}

#[test]
fn test_parse_simple_code_samples() -> Result<(), tree_sitter::LanguageError> {
    let samples = [
        (Lang::Rust, "fn main() {}"),
        (Lang::Python, "def main():\n    pass"),
        (Lang::Toml, "[package]\nname = \"test\""),
    ];

    for (lang, code) in samples {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&lang.to_tree_sitter())?;
        let Some(tree) = parser.parse(code, None) else {
            panic!("Failed to parse {lang:?} code: {code}");
        };

        let root = tree.root_node();
        assert!(
            root.child_count() > 0 || !root.byte_range().is_empty(),
            "Empty parse tree for {lang:?}"
        );
        assert!(!root.has_error(), "{lang:?} sample parsed with errors: {code}");
    }
    Ok(())
}
