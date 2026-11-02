use super::*;

#[test]
fn an_unknown_lint_has_rustcs_suggestion() {
    let store = Store::default();
    let suggested = |name: &str, tool: Option<&str>| {
        assert_eq!(store.check(name, tool), Checked::NoLint { suggest: true }, "{name:?} of {tool:?}");
        suggestion(&store, name, tool)
    };
    assert_eq!(suggested("dead_cod", None), Some(("dead_code".to_owned(), false)));
    assert_eq!(suggested("DEAD_CODE", None), Some(("dead_code".to_owned(), false)));
    let store = Store { internal: true, registered_tools: Vec::new() };
    assert_eq!(store.check("default_hash_type", Some("rustc")), Checked::NoLint { suggest: true });
    assert_eq!(suggestion(&store, "default_hash_type", Some("rustc")), Some(("rustc::default_hash_types".to_owned(), false)));
}
