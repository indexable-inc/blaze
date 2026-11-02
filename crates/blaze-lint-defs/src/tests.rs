use edition::Edition;

use super::*;

#[test]
fn lint_ids_are_dense_and_round_trip() {
    let all = lints().iter().chain(blaze_lints()).chain(internal_lints()).chain(unregistered());
    let mut count = 0;
    for (at, lint) in all.enumerate() {
        let id = LintId::of(lint);
        assert_eq!(usize::from(id.0), at, "{}", lint.name);
        assert_eq!(id.index(), at, "{}", lint.name);
        assert_eq!(lint.id(), id, "{}", lint.name);
        assert!(std::ptr::eq(id.lint(), lint), "{}", lint.name);
        assert!(std::ptr::eq(id.alone()[0], lint), "{}", lint.name);
        assert_eq!(id.alone().len(), 1);
        // A copy of a lint keeps its id.
        let copy = *lint;
        assert_eq!(LintId::of(&copy), id, "{}", lint.name);
        count += 1;
    }
    assert_eq!(count, index::LINT_COUNT);
    // The tables are the declared ones, in their order: only the ids were added.
    let declared = [lints::BUILTIN, lints::BLAZE, lints::INTERNAL, lints::UNREGISTERED].concat();
    for (lint, declared) in ALL.iter().zip(&declared) {
        assert_eq!(Lint { id: LintId::UNNUMBERED, ..*lint }, *declared, "{}", lint.name);
    }
}

#[test]
fn named_ids_are_the_lints_of_their_names() {
    let named = [
        (LintId::WARNINGS, "warnings"),
        (LintId::UNFULFILLED_LINT_EXPECTATIONS, "unfulfilled_lint_expectations"),
        (LintId::RENAMED_AND_REMOVED_LINTS, "renamed_and_removed_lints"),
        (LintId::UNKNOWN_LINTS, "unknown_lints"),
        (LintId::UNUSED_ATTRIBUTES, "unused_attributes"),
        (LintId::FORBIDDEN_LINT_GROUPS, "forbidden_lint_groups"),
        (LintId::DEAD_CODE, "dead_code"),
        (LintId::SINGLE_USE_LIFETIMES, "single_use_lifetimes"),
        (LintId::MISSING_DOCS, "missing_docs"),
    ];
    for (id, name) in named {
        let found = lints().iter().find(|it| it.name == name).expect("a builtin lint");
        assert!(std::ptr::eq(id.lint(), found), "{name}");
        assert_eq!(id.lint().name, name);
    }
}

#[test]
fn blaze_is_a_tool_with_its_lints_and_group() {
    let store = Store::default();
    let lint = lint("blaze::const_impure").expect("blaze's lint");
    assert_eq!(lint.default_level, Level::Warn);
    assert_eq!(blaze_lints().len(), 20);
    assert!(!lint.is_externally_loaded);
    assert!(blaze_lints()[1..].iter().all(|it| it.default_level == Level::Allow && it.is_externally_loaded));
    let hot = group("blaze::hot").expect("blaze's group");
    let members: Vec<&str> = hot.lints.iter().map(|it| it.name).collect();
    assert_eq!(
        members,
        ["blaze::hot_clone", "blaze::hot_alloc", "blaze::hot_alloc_call", "blaze::hot_dyn", "blaze::std_hashmap"]
    );
    assert_eq!(store.check("hot", Some(BLAZE_TOOL)), Checked::Tool(&hot.lints, None));
    let hot_dyn = super::lint("blaze::hot_dyn").expect("blaze-lints' lint");
    assert_eq!(store.check("hot_dyn", Some(BLAZE_TOOL)), Checked::Tool(hot_dyn.id().alone(), None));
    assert_eq!(store.group_of(hot_dyn), None);
    assert_eq!(store.check("const_impure", Some(BLAZE_TOOL)), Checked::Tool(lint.id().alone(), None));
    assert_eq!(store.find_lints("blaze::const_impure"), Some(lint.id().alone()));
    assert!(matches!(store.check("const_purity", Some(BLAZE_TOOL)), Checked::NoLint { .. }));
    assert_eq!(store.check("const_impure", Some("blazer")), Checked::NoTool);
    assert_eq!(store.group_of(lint), None);
}

#[test]
fn group_of_is_the_first_group_without_aliases() {
    let store = Store::default();
    for lint in lints() {
        let first = groups().iter().filter(|it| it.alias_of.is_none()).find(|it| it.lints.iter().any(|it| it.name == lint.name));
        assert_eq!(store.group_of(lint), first.map(|it| it.name), "{}", lint.name);
    }
}

#[test]
fn counts_are_rustcs() {
    // 108 of the builtin passes and `foreign_modules`, 141 hardwired.
    assert_eq!(lints().len(), 249);
    assert_eq!(internal_lints().len(), 17);
    assert_eq!(internal_group().lints.len(), 16);
    assert_eq!(unregistered().len(), 2);
    let names: Vec<&str> = groups().iter().map(|it| it.name).collect();
    assert_eq!(
        names,
        [
            "rust_2018_compatibility",
            "rust_2024_compatibility",
            "rust_2021_compatibility",
            "future_incompatible",
            "nonstandard_style",
            "unused",
            "let_underscore",
            "rust_2018_idioms",
            "keyword_idents",
            "refining_impl_trait",
            "deprecated_safe",
            "unknown_or_malformed_diagnostic_attributes",
            "bad_style",
            "blaze::hot",
        ]
    );
    assert_eq!(group("unused").unwrap().lints.len(), 24);
    assert_eq!(renamed().len(), 18);
    assert_eq!(removed().len(), 66);
}

#[test]
fn names_are_unique_and_lowercase() {
    let mut seen = std::collections::HashSet::new();
    for lint in lints().iter().chain(internal_lints()).chain(unregistered()) {
        assert!(seen.insert(lint.name), "`{}` twice", lint.name);
        assert_eq!(lint.name, lint.name.to_lowercase());
    }
    for (old, new) in renamed() {
        assert!(lint(old).is_none(), "`{old}` is renamed and a lint");
        assert!(lint(new).is_some(), "`{old}` renamed to no lint `{new}`");
    }
}

#[test]
fn a_lint() {
    let it = lint("dead_code").unwrap();
    assert_eq!(it.default_level, Level::Warn);
    assert_eq!(it.desc, "detect unused, unexported items");
    assert!(!it.report_in_external_macro);
    assert!(lint("unreachable_code").unwrap().report_in_external_macro);
    assert!(lint("unused_crate_dependencies").unwrap().crate_level_only);
    assert!(lint("DEAD_CODE").is_none());
    assert_eq!(named("dead_code"), Named::Lint(it));
    assert_eq!(declared("private_macro_use").unwrap().default_level, Level::Deny);
    assert_eq!(named("private_macro_use"), Named::Unknown);
}

#[test]
fn edition_levels() {
    let it = lint("unsafe_op_in_unsafe_fn").unwrap();
    assert_eq!(it.default_level(Edition::Edition2021), Level::Allow);
    assert_eq!(it.default_level(Edition::Edition2024), Level::Warn);
    let it = lint("static_mut_refs").unwrap();
    assert_eq!(it.default_level(Edition::Edition2015), Level::Warn);
    assert_eq!(it.default_level(Edition::Edition2024), Level::Deny);
}

#[test]
fn future_incompatible() {
    let it = lint("ambiguous_glob_imports").unwrap().future_incompatible.unwrap();
    assert_eq!(it.reason, Reason::FutureReleaseError(114095));
    assert!(it.report_in_deps);
    assert_eq!(
        it.reason.reference(),
        "issue #114095 <https://github.com/rust-lang/rust/issues/114095>"
    );
    assert_eq!(
        it.reason.explanation(Edition::Edition2021),
        "this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!"
    );
    let it = lint("bare_trait_objects").unwrap().future_incompatible.unwrap();
    assert_eq!(it.reason.edition(), Some(Edition::Edition2021));
    assert_eq!(
        it.reason.reference(),
        "<https://doc.rust-lang.org/edition-guide/rust-2021/warnings-promoted-to-error.html>"
    );
    assert_eq!(
        it.reason.explanation(Edition::Edition2018),
        "this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2021!"
    );
    assert!(!lint("static_mut_refs").unwrap().future_incompatible.unwrap().explain_reason);
    let groups = |name: &str| group(name).unwrap().lints.iter().any(|it| it.name == "bare_trait_objects");
    assert!(groups("rust_2021_compatibility"));
    assert!(!groups("future_incompatible"));
    assert!(groups("rust_2018_idioms"));
}

#[test]
fn renamed_and_removed() {
    assert_eq!(named("bare_trait_object"), Named::Renamed("bare_trait_objects"));
    assert_eq!(named("raw_pointer_derive"), Named::Removed("using derive with raw pointers is ok"));
    assert_eq!(named("broken_intra_doc_links"), Named::Ignored);
    assert_eq!(named("no_such_lint"), Named::Unknown);
    assert!(matches!(named("nonstandard_style"), Named::Group(_)));
    assert_eq!(group("bad_style").unwrap().alias_of, Some("nonstandard_style"));
    let store = Store::default();
    assert_eq!(store.check("bare_trait_object", None), Checked::Renamed("bare_trait_objects"));
    assert_eq!(
        store.check("raw_pointer_derive", None),
        Checked::Removed("using derive with raw pointers is ok")
    );
}

#[test]
fn check_lint_name() {
    let store = Store::default();
    let dead = lint("dead_code").unwrap();
    assert_eq!(store.check("dead_code", None), Checked::Ok(dead.id().alone()));
    assert_eq!(store.check("broken_intra_doc_links", None), Checked::Ok(&[]));
    assert_eq!(store.check("dead_cod", None), Checked::NoLint { suggest: true });
    assert_eq!(store.check("DEAD_CODE", None), Checked::NoLint { suggest: true });
    assert_eq!(store.check("foo", Some("bar")), Checked::NoTool);
    assert_eq!(store.check("needless_return", Some("clippy")), Checked::MissingTool);
    assert_eq!(store.check("anything", Some("rustdoc")), Checked::MissingTool);
    assert_eq!(store.check("internal", Some("rustc")), Checked::MissingTool);
    let store = Store { internal: true, registered_tools: Vec::new() };
    assert!(matches!(store.check("internal", Some("rustc")), Checked::Tool(lints, None) if lints.len() == 16));
    assert_eq!(
        store.check("default_hash_type", Some("rustc")),
        Checked::NoLint { suggest: true }
    );
    assert!(store.is_lint_group("warnings"));
    assert!(store.is_lint_group("unused"));
    assert!(!store.is_lint_group("dead_code"));
}

/// The registry's lookups as they were before the perfect hashes: scans of the tables, with
/// rustc's store's rules (a removed name over a renamed one over an ignored one, the first
/// group of a name). Each perfect-hash lookup is checked against these.
mod by_scan {
    use super::*;

    fn find(tables: &[&'static [Lint]], name: &str) -> Option<&'static Lint> {
        tables.iter().flat_map(|it| it.iter()).find(|it| it.name == name)
    }

    pub(super) fn lint(name: &str) -> Option<&'static Lint> {
        find(&[lints(), blaze_lints()], name)
    }

    pub(super) fn declared(name: &str) -> Option<&'static Lint> {
        lint(name).or_else(|| find(&[internal_lints(), unregistered()], name))
    }

    /// The groups as `build_groups` made them before: by name, as each lint is registered.
    pub(super) fn groups() -> Vec<(&'static str, Vec<&'static Lint>, Option<&'static str>)> {
        let mut groups: Vec<(&'static str, Vec<&'static Lint>, Option<&'static str>)> = Vec::new();
        for lint in lints() {
            if let Some(future) = lint.future_incompatible {
                let name = match future.reason.edition() {
                    Some(edition) => edition_group(edition),
                    None => "future_incompatible",
                };
                match groups.iter_mut().find(|it| it.0 == name) {
                    Some(group) => group.1.push(lint),
                    None => groups.push((name, vec![lint], None)),
                }
            }
        }
        for &(name, members) in names::GROUPS {
            let lints = members.iter().map(|member| lints().iter().find(|it| it.name == *member).unwrap()).collect();
            groups.push((name, lints, None));
        }
        for &(alias, of) in names::ALIASES {
            let lints = groups.iter().find(|it| it.0 == of).unwrap().1.clone();
            groups.push((alias, lints, Some(of)));
        }
        for &(name, members) in names::BLAZE_GROUPS {
            let lints = members.iter().map(|member| blaze_lints().iter().find(|it| it.name == *member).unwrap()).collect();
            groups.push((name, lints, None));
        }
        groups
    }

    pub(super) fn group(name: &str) -> Option<&'static Group> {
        super::groups().iter().find(|it| it.name == name)
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Name {
        Renamed(&'static str),
        Removed(&'static str),
        Ignored,
    }

    /// rustc's `by_name` of a name that is not a lint's nor a group's: the last registered.
    pub(super) fn renaming(name: &str) -> Option<Name> {
        if let Some(&(_, reason)) = names::REMOVED.iter().find(|it| it.0 == name) {
            return Some(Name::Removed(reason));
        }
        if let Some(&(_, new)) = names::RENAMED.iter().find(|it| it.0 == name) {
            return Some(Name::Renamed(new));
        }
        names::IGNORED.contains(&name).then_some(Name::Ignored)
    }

    pub(super) fn named(name: &str) -> Named {
        if let Some(lint) = lint(name) {
            return Named::Lint(lint);
        }
        if let Some(group) = group(name) {
            return Named::Group(group);
        }
        match renaming(name) {
            Some(Name::Renamed(new)) => Named::Renamed(new),
            Some(Name::Removed(reason)) => Named::Removed(reason),
            Some(Name::Ignored) => Named::Ignored,
            None => Named::Unknown,
        }
    }

    pub(super) fn by_name(store: &Store, name: &str) -> Option<&'static Lint> {
        lint(name).or_else(|| store.internal.then(|| internal_lints().iter().find(|it| it.name == name)).flatten())
    }

    pub(super) fn store_group(store: &Store, name: &str) -> Option<&'static Group> {
        group(name).or_else(|| (store.internal && name == "rustc::internal").then(internal_group))
    }

    pub(super) fn find_lints(store: &Store, name: &str) -> Option<Vec<&'static Lint>> {
        if let Some(lint) = by_name(store, name) {
            return Some(vec![lint]);
        }
        match renaming(name) {
            Some(Name::Renamed(new)) => return Some(vec![by_name(store, new)?]),
            Some(Name::Removed(_)) => return None,
            Some(Name::Ignored) => return Some(Vec::new()),
            None => {}
        }
        store_group(store, name).map(|it| it.lints.clone())
    }

    pub(super) fn has_lints_of(store: &Store, tool: &str) -> bool {
        let of = |table: &[Lint]| table.iter().any(|it| it.name.split_once("::").is_some_and(|(it, _)| it == tool));
        of(lints()) || of(blaze_lints()) || (store.internal && of(internal_lints()))
    }

    /// [`Checked`] with its lists by name, to compare two of them.
    #[derive(Debug, PartialEq, Eq)]
    pub(super) enum Plain {
        Ok(Vec<&'static str>),
        NoLint(bool),
        NoTool,
        Renamed(&'static str),
        Removed(&'static str),
        Tool(Vec<&'static str>, Option<String>),
        MissingTool,
    }

    fn names_of(lints: &[&'static Lint]) -> Vec<&'static str> {
        lints.iter().map(|it| it.name).collect()
    }

    pub(super) fn plain(checked: Checked) -> Plain {
        match checked {
            Checked::Ok(lints) => Plain::Ok(names_of(lints)),
            Checked::NoLint { suggest } => Plain::NoLint(suggest),
            Checked::NoTool => Plain::NoTool,
            Checked::Renamed(it) => Plain::Renamed(it),
            Checked::Removed(it) => Plain::Removed(it),
            Checked::Tool(lints, new) => Plain::Tool(names_of(lints), new),
            Checked::MissingTool => Plain::MissingTool,
        }
    }

    /// `Store::check` as it was, with a `String` for the complete name.
    pub(super) fn check(store: &Store, name: &str, tool: Option<&str>) -> Plain {
        if let Some(tool) = tool
            && tool != "rustc"
            && tool != "rustdoc"
            && tool != BLAZE_TOOL
            && !PREDEFINED_TOOLS.contains(&tool)
            && !store.registered_tools.iter().any(|it| it == tool)
        {
            return Plain::NoTool;
        }
        let complete = match tool {
            Some(tool) => format!("{tool}::{name}"),
            None => name.to_owned(),
        };
        if let Some(tool) = tool {
            if let Some(lint) = by_name(store, &complete) {
                return Plain::Tool(vec![lint.name], None);
            }
            if renaming(&complete).is_none() {
                return match store_group(store, &complete) {
                    Some(group) => Plain::Tool(names_of(&group.lints), None),
                    None if has_lints_of(store, tool) => Plain::NoLint(true),
                    None => Plain::MissingTool,
                };
            }
        }
        if let Some(lint) = by_name(store, &complete) {
            return Plain::Ok(vec![lint.name]);
        }
        match renaming(&complete) {
            Some(Name::Renamed(new)) => Plain::Renamed(new),
            Some(Name::Removed(reason)) => Plain::Removed(reason),
            Some(Name::Ignored) => Plain::Ok(Vec::new()),
            None => match store_group(store, &complete) {
                Some(group) => Plain::Ok(names_of(&group.lints)),
                None => {
                    let complete = format!("clippy::{complete}");
                    if let Some(lint) = by_name(store, &complete) {
                        return Plain::Tool(vec![lint.name], Some(complete));
                    }
                    if renaming(&complete).is_some() {
                        return Plain::NoLint(false);
                    }
                    match store_group(store, &complete) {
                        Some(group) => Plain::Tool(names_of(&group.lints), Some(complete)),
                        None => Plain::NoLint(true),
                    }
                }
            },
        }
    }
}

/// Every name the store has: of every lint table, every group and alias, `rustc::internal`,
/// the renamed (old and new), removed and ignored names.
fn every_name() -> Vec<&'static str> {
    let mut names: Vec<&'static str> =
        lints().iter().chain(blaze_lints()).chain(internal_lints()).chain(unregistered()).map(|it| it.name).collect();
    names.extend(groups().iter().map(|it| it.name));
    names.push("rustc::internal");
    names.extend(names::RENAMED.iter().flat_map(|&(old, new)| [old, new]));
    names.extend(names::REMOVED.iter().map(|it| it.0));
    names.extend(names::IGNORED);
    names
}

/// Names near `name` that are none of the store's (or, rarely, another's: each is checked
/// against the scan either way).
fn near_misses(name: &str) -> Vec<String> {
    let mut out = vec![
        format!("{name}_"),
        format!("_{name}"),
        name[..name.len() - 1].to_owned(),
        name[1..].to_owned(),
        name.to_uppercase(),
        format!("{name}\0"),
        format!("{name} "),
        format!("clippy::{name}"),
        format!("rustc::{name}"),
        format!("blaze::{name}"),
    ];
    if let Some((_, rest)) = name.split_once("::") {
        out.push(rest.to_owned());
    }
    let mut flipped = name.as_bytes().to_vec();
    flipped[0] ^= 0x20;
    out.extend(String::from_utf8(flipped).ok());
    out
}

fn names_and_misses() -> Vec<String> {
    let mut names: Vec<String> = every_name().into_iter().map(str::to_owned).collect();
    let misses: Vec<String> = every_name().into_iter().flat_map(near_misses).collect();
    names.extend(misses);
    names.extend(["", "warnings", "unused", "bad_style", "rustc", "clippy::", "::", "a"].map(str::to_owned));
    // Longer than any name of the tables, with and without a tool.
    names.push("x".repeat(index::LONGEST + 1));
    names.push(format!("{}dead_code", "x".repeat(index::LONGEST)));
    names
}

#[test]
fn every_name_is_found_as_by_a_scan() {
    for name in &names_and_misses() {
        let name = name.as_str();
        assert_eq!(lint(name).map(|it| it.name), by_scan::lint(name).map(|it| it.name), "{name:?}");
        assert!(lint(name).zip(by_scan::lint(name)).is_none_or(|(a, b)| std::ptr::eq(a, b)), "{name:?}");
        assert_eq!(declared(name).map(|it| it.name), by_scan::declared(name).map(|it| it.name), "{name:?}");
        assert!(declared(name).zip(by_scan::declared(name)).is_none_or(|(a, b)| std::ptr::eq(a, b)), "{name:?}");
        let group = group(name);
        assert_eq!(group.map(|it| it.name), by_scan::group(name).map(|it| it.name), "{name:?}");
        assert!(group.zip(by_scan::group(name)).is_none_or(|(a, b)| std::ptr::eq(a, b)), "{name:?}");
        assert_eq!(named(name), by_scan::named(name), "{name:?}");
        let renamed = match by_scan::renaming(name) {
            Some(by_scan::Name::Renamed(new)) => Some(new),
            _ => None,
        };
        assert_eq!(renamed_to(name), renamed, "{name:?}");
        assert_eq!(
            renaming(name.as_bytes()).map(|it| format!("{it:?}")),
            by_scan::renaming(name).map(|it| format!("{it:?}")),
            "{name:?}"
        );
        for internal in [false, true] {
            let store = Store { internal, registered_tools: vec!["tool".to_owned()] };
            assert_eq!(
                store.by_name(name.as_bytes()).map(|it| it.name),
                by_scan::by_name(&store, name).map(|it| it.name),
                "{name:?} internal: {internal}"
            );
            assert_eq!(
                store.group(name.as_bytes()).map(|it| it.name),
                by_scan::store_group(&store, name).map(|it| it.name),
                "{name:?} internal: {internal}"
            );
            assert_eq!(
                store.find_lints(name).map(|it| it.to_vec()),
                by_scan::find_lints(&store, name),
                "{name:?} internal: {internal}"
            );
            assert_eq!(
                store.is_lint_group(name),
                name == "warnings" || by_scan::store_group(&store, name).is_some(),
                "{name:?}"
            );
        }
    }
}

#[test]
fn check_is_as_by_a_scan() {
    let mut names: Vec<String> = every_name().into_iter().map(str::to_owned).collect();
    let misses: Vec<String> = every_name().into_iter().flat_map(near_misses).collect();
    names.extend(misses);
    names.extend(["", "warnings", "unused", "bad_style", "rustc", "clippy::", "::", "a"].map(str::to_owned));
    names.push("x".repeat(index::LONGEST + 1));
    let tools = [None, Some("rustc"), Some("rustdoc"), Some("blaze"), Some("clippy"), Some("tool"), Some("nope")];
    for internal in [false, true] {
        let store = Store { internal, registered_tools: vec!["tool".to_owned()] };
        for name in &names {
            // The checks of a name's tool part: `tool::name` written as rustc's attribute
            // parser splits it, and as given.
            let mut asked: Vec<(&str, Option<&str>)> = tools.iter().map(|&tool| (name.as_str(), tool)).collect();
            if let Some((tool, rest)) = name.split_once("::") {
                asked.push((rest, Some(tool)));
            }
            for (name, tool) in asked {
                assert_eq!(
                    by_scan::plain(store.check(name, tool)),
                    by_scan::check(&store, name, tool),
                    "{name:?} of {tool:?}, internal: {internal}"
                );
            }
        }
    }
}

#[test]
fn groups_are_as_built_by_name() {
    let built = by_scan::groups();
    assert_eq!(groups().len(), built.len());
    assert_eq!(groups().len(), index::GROUP_COUNT);
    for (group, (name, lints, alias_of)) in groups().iter().zip(&built) {
        assert_eq!(group.name, *name);
        assert_eq!(group.alias_of, *alias_of);
        assert_eq!(group.lints.len(), lints.len(), "{name}");
        for (a, b) in group.lints.iter().zip(lints) {
            assert!(std::ptr::eq(*a, *b), "{name}: {} and {}", a.name, b.name);
        }
    }
    let all = lints().iter().chain(blaze_lints()).chain(internal_lints()).chain(unregistered());
    for group in groups().iter().chain([internal_group()]) {
        for lint in all.clone() {
            let listed = group.lints.iter().any(|it| std::ptr::eq(*it, lint));
            assert_eq!(group.has(lint.id()), listed, "{} of {}", lint.name, group.name);
        }
        assert_eq!(group.members().iter().count(), group.lints.len(), "{}", group.name);
    }
}

#[test]
fn group_of_an_internal_lint() {
    let store = Store { internal: true, registered_tools: Vec::new() };
    for lint in internal_lints() {
        let first = internal_group().lints.iter().any(|it| std::ptr::eq(*it, lint)).then_some("rustc::internal");
        assert_eq!(store.group_of(lint), first, "{}", lint.name);
        assert_eq!(Store::default().group_of(lint), None, "{}", lint.name);
    }
    for lint in blaze_lints().iter().chain(unregistered()) {
        assert_eq!(store.group_of(lint), None, "{}", lint.name);
    }
}

#[test]
fn tools_are_as_by_a_scan() {
    let mut tools: Vec<&str> = vec!["", "rustc", "rustdoc", "blaze", "clippy", "rust", "blaze:", "rustc::", "r"];
    tools.extend(every_name().into_iter().filter_map(|it| it.split_once("::").map(|(tool, _)| tool)));
    for internal in [false, true] {
        let store = Store { internal, registered_tools: Vec::new() };
        for tool in &tools {
            assert_eq!(store.has_lints_of(tool), by_scan::has_lints_of(&store, tool), "{tool:?} internal: {internal}");
        }
    }
    assert!(Store { internal: true, registered_tools: Vec::new() }.has_lints_of("rustc"));
    assert!(!Store::default().has_lints_of("rustc"));
    assert!(Store::default().has_lints_of(BLAZE_TOOL));
    for tool in ["rustc", "rustdoc", "blaze", "clippy", "rustfmt", "diagnostic", "miri", "rust_analyzer"] {
        assert!(KNOWN_TOOLS.get(tool).is_some(), "{tool}");
        for miss in near_misses(tool) {
            let known = ["rustc", "rustdoc", BLAZE_TOOL].contains(&miss.as_str()) || PREDEFINED_TOOLS.contains(&miss.as_str());
            assert_eq!(KNOWN_TOOLS.get(&miss).is_some(), known, "{miss:?}");
        }
    }
}

#[test]
fn joined_names_are_the_bytes_of_the_format() {
    for (tool, name) in [("clippy", "x"), ("rustc", "default_hash_types"), ("", ""), ("a", "b::c")] {
        assert_eq!(index::Joined::new(tool, name).bytes(), format!("{tool}::{name}").as_bytes());
    }
    let long = "x".repeat(index::LONGEST);
    assert_eq!(index::Joined::new("t", &long).bytes(), b"");
}

#[test]
fn level_attrs_are_as_matched() {
    let by_match = |name: &str| match name {
        "allow" => Some(Level::Allow),
        "expect" => Some(Level::Expect),
        "warn" => Some(Level::Warn),
        "deny" => Some(Level::Deny),
        "forbid" => Some(Level::Forbid),
        _ => None,
    };
    let mut names: Vec<String> = ["allow", "expect", "warn", "deny", "forbid", "force-warn", "force_warn", "doc", "cfg", ""]
        .map(str::to_owned)
        .to_vec();
    let misses: Vec<String> = ["allow", "expect", "warn", "deny", "forbid"].into_iter().flat_map(near_misses).collect();
    names.extend(misses);
    for name in &names {
        assert_eq!(Level::of_attr(name), by_match(name), "{name:?}");
    }
}

#[test]
fn lint_sets_are_as_sets() {
    let mut rng = blaze_ids::rng::SplitMix64::new(0x6c69_6e74_7365_74);
    let mut set = LintSet::EMPTY;
    let mut reference = std::collections::BTreeSet::new();
    for _ in 0..10_000 {
        let id = LintId(rng.index(index::LINT_COUNT) as u16);
        match rng.below(3) {
            0 => {
                set.remove(id);
                reference.remove(&id);
            }
            _ => {
                set.insert(id);
                reference.insert(id);
            }
        }
        assert_eq!(set.contains(id), reference.contains(&id));
    }
    assert_eq!(set.iter().collect::<Vec<_>>(), reference.iter().copied().collect::<Vec<_>>());
    assert_eq!(set.is_empty(), reference.is_empty());
    assert_eq!(reference.iter().copied().collect::<LintSet>(), set);
    let last = LintId((index::LINT_COUNT - 1) as u16);
    let mut edges = LintSet::EMPTY;
    edges.insert(LintId(0));
    edges.insert(LintId(63));
    edges.insert(LintId(64));
    edges.insert(last);
    assert_eq!(edges.iter().collect::<Vec<_>>(), [LintId(0), LintId(63), LintId(64), last]);
    assert!(LintSet::EMPTY.is_empty());
}

#[test]
fn lint_options_are_rustcs_flags_in_order() {
    let args = |args: &[&str]| -> Vec<String> { args.iter().map(|&it| it.to_owned()).collect() };
    let options = LintOptions::parse(&args(&[
        "-A", "dead-code", "-Wunused", "--force-warn", "x", "--deny=clippy::all", "--forbid", "y", "--cap-lints", "warn",
    ]))
    .unwrap();
    assert_eq!(
        options.opts,
        [
            ("dead_code".to_owned(), Level::Allow),
            ("unused".to_owned(), Level::Warn),
            ("x".to_owned(), Level::ForceWarn),
            ("clippy::all".to_owned(), Level::Deny),
            ("y".to_owned(), Level::Forbid),
        ]
    );
    assert_eq!(options.cap, Some(Level::Warn));
    assert_eq!(LintOptions::parse(&args(&[])).unwrap(), LintOptions::default());
    assert_eq!(LintOptions::parse(&args(&["-D"])).unwrap_err(), "Argument to option 'D' missing");
    assert_eq!(
        LintOptions::parse(&args(&["--cap-lints", "allow", "--cap-lints=deny"])).unwrap_err(),
        "Option 'cap-lints' given more than once"
    );
    assert_eq!(LintOptions::parse(&args(&["--cap-lints", "expect"])).unwrap_err(), "unknown lint level: `expect`");
    assert_eq!(LintOptions::parse(&args(&["--cfg", "x"])).unwrap_err(), "`--cfg` is not a lint flag");
    assert!(LintOptions::parse(&args(&["-W", "help"])).is_err());
}
