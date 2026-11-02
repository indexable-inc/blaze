//! Owners of a build's functions and statics, and the refusal of a dependency's code that
//! refers to the workspace's.

use super::*;

fn key(n: u8) -> InstanceKey {
    InstanceKey(ContentId([n; 32]))
}

fn item(n: u8) -> ItemKey {
    ItemKey(ContentId([n + 100; 32]))
}

fn code(n: u8) -> CodeHash {
    CodeHash(ContentId([n + 200; 32]))
}

fn function(n: u8, owner: Owner, refs: &[Target]) -> Function<'_> {
    Function { key: key(n), item: item(n), owner, code: code(n), refs, name: "f", origin: Origin::Std }
}

fn static_of<'a>(id: (ItemKey, ContentId), unit: InstanceKey, owner: Owner, refs: &'a [Target]) -> StaticOf<'a> {
    StaticOf { id, unit, owner, refs, symbol: None, origin: Origin::Std }
}

#[test]
fn an_origin_owns_what_no_type_argument_makes_another_s() {
    assert_eq!(Owner::of(&Origin::Std), Owner::Dependency);
    use crate::plan::Package;
    assert_eq!(Owner::of(&Origin::Dependency(Package::new("regex"))), Owner::Dependency);
    assert_eq!(Owner::of(&Origin::Member(Package::new("app"))), Owner::Workspace);
    assert_eq!(Owner::of(&Origin::BuildTime(Package::new("serde_derive"))), Owner::Workspace);
    assert!(Owner::Dependency < Owner::Workspace, "of two describers, the image's wins");
}

#[test]
fn owners_are_by_key_with_the_generic_s_code() {
    let none: [Target; 0] = [];
    let functions = [function(1, Owner::Workspace, &none), function(2, Owner::Dependency, &none)];
    let static_id = (item(9), ContentId::default());
    let statics = [static_of(static_id, key(9), Owner::Dependency, &none)];
    let mut errors = Vec::new();
    let owned = owners(functions.into_iter(), statics.into_iter(), &mut errors);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(owned.functions, [(key(1), Owner::Workspace, code(1)), (key(2), Owner::Dependency, code(2))]);
    assert_eq!(owned.statics, [(item(9), ContentId::default(), Owner::Dependency)]);
}

#[test]
fn the_workspace_may_call_a_dependency_and_a_dependency_may_not_call_the_workspace() {
    let calls_dependency = [Target::Direct(key(2)), Target::Extern("memcpy".to_owned())];
    let calls_workspace = [Target::Function(key(1))];
    let none: [Target; 0] = [];
    let mut errors = Vec::new();
    let fine = [function(1, Owner::Workspace, &calls_dependency), function(2, Owner::Dependency, &none)];
    owners(fine.into_iter(), std::iter::empty(), &mut errors);
    assert!(errors.is_empty(), "{errors:?}");

    let wrong = [function(1, Owner::Workspace, &none), function(2, Owner::Dependency, &calls_workspace)];
    owners(wrong.into_iter(), std::iter::empty(), &mut errors);
    assert_eq!(errors.len(), 1);
    assert_eq!((errors[0].unit, errors[0].item), (key(2), item(2)));
    assert!(errors[0].what.contains("the workspace's"), "{}", errors[0].what);
}

#[test]
fn a_dependency_s_static_and_a_reference_to_a_workspace_static_are_checked() {
    let workspace_static = (item(8), ContentId([1; 32]));
    let names_static = [Target::Static { item: workspace_static.0, instance: workspace_static.1 }];
    let names_function = [Target::Direct(key(1))];
    let none: [Target; 0] = [];
    let functions = [function(1, Owner::Workspace, &none), function(2, Owner::Dependency, &names_static)];
    let statics = [
        static_of(workspace_static, key(8), Owner::Workspace, &none),
        static_of((item(9), ContentId::default()), key(9), Owner::Dependency, &names_function),
    ];
    let mut errors = Vec::new();
    owners(functions.into_iter(), statics.into_iter(), &mut errors);
    let named: Vec<_> = errors.iter().map(|it| (it.unit, it.item)).collect();
    assert_eq!(named, [(key(2), item(2)), (key(9), item(9))]);
}

/// Checked on every core, the refusals are the ones a check in key order makes, in that order.
#[test]
fn refusals_across_cores_come_in_key_order() {
    let numbered = |n: u32, tag: u8| {
        let mut bytes = [tag; 32];
        bytes[..4].copy_from_slice(&n.to_be_bytes());
        ContentId(bytes)
    };
    let workspace = InstanceKey(numbered(u32::MAX, 0));
    let calls_workspace = [Target::Direct(workspace)];
    let none: [Target; 0] = [];
    let functions: Vec<Function<'_>> = (0..3000u32)
        .map(|n| Function {
            key: InstanceKey(numbered(n, 0)),
            item: ItemKey(numbered(n, 1)),
            owner: Owner::Dependency,
            code: CodeHash(numbered(n, 2)),
            refs: if n % 3 == 1 { &calls_workspace } else { &none },
            name: "f",
            origin: Origin::Std,
        })
        .chain(std::iter::once(Function {
            key: workspace,
            item: ItemKey(numbered(u32::MAX, 1)),
            owner: Owner::Workspace,
            code: CodeHash(numbered(u32::MAX, 2)),
            refs: &none,
            name: "g",
            origin: Origin::Std,
        }))
        .collect();
    let mut errors = Vec::new();
    owners(functions.iter().copied(), std::iter::empty(), &mut errors);
    let named: Vec<_> = errors.iter().map(|it| it.unit).collect();
    let expected: Vec<_> = (0..3000u32).filter(|n| n % 3 == 1).map(|n| InstanceKey(numbered(n, 0))).collect();
    assert_eq!(named, expected);
}

/// A refusal names the dependency's code and the workspace's it refers to, each by its name
/// and package, so the run that hits it says which they are.
#[test]
fn a_refusal_names_both_sides_and_their_packages() {
    use crate::plan::Package;
    let calls_workspace = [Target::Direct(key(1))];
    let names_static = [Target::Static { item: item(8), instance: ContentId::default() }];
    let none: [Target; 0] = [];
    let functions = [
        Function { name: "app::helper", origin: Origin::BuildTime(Package::new("app")), ..function(1, Owner::Workspace, &none) },
        Function { name: "std::rt::run::{closure#0}", ..function(2, Owner::Dependency, &calls_workspace) },
    ];
    let statics = [
        StaticOf { symbol: Some("APP_TABLE"), origin: Origin::Member(Package::new("app")), ..static_of((item(8), ContentId::default()), key(8), Owner::Workspace, &none) },
        StaticOf { origin: Origin::Dependency(Package::new("regex")), ..static_of((item(9), ContentId::default()), key(9), Owner::Dependency, &names_static) },
    ];
    let mut errors = Vec::new();
    owners(functions.into_iter(), statics.into_iter(), &mut errors);
    assert_eq!(errors.len(), 2, "{errors:?}");
    let function = &errors[0].what;
    assert!(function.starts_with("`std::rt::run::{closure#0}` (of the standard library) is a dependency's"), "{function}");
    assert!(function.contains("refers to `app::helper` (of build-time `app`), Direct("), "{function}");
    let static_ = &errors[1].what;
    assert!(static_.starts_with("a static (of dependency `regex`) is a dependency's"), "{static_}");
    assert!(static_.contains("refers to the static `APP_TABLE` (of member `app`), Static {"), "{static_}");
}
