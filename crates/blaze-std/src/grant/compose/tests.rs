//! Composed grants (compose.rs) through the launcher's test entry ([`launch_in`]) on the
//! simulation: each form granted and not granted, and each form's manifest JSON. `main`'s
//! status says what it was handed (`Status(n)`, n chosen per case), so nothing is kept aside.
//! Compile-time refusals are fixture pairs under crates/blaze-std/refusals/ (`grant_maybe_maybe`,
//! `grant_one_of_same_kind`, `grant_one_of_read_then_write`, `grant_many_fills_itself`).

use super::{Chosen2, Connect, Listen, Many, Maybe, OneOf};
use crate::{
    cx::Cx,
    entry::{Status, launch_in},
    fs::{Dir, ReadOnly, ReadWrite},
    grant::{LaunchAlloc, Manifest, Param},
    io::Io,
    spec::{GrantName, Granted, RunSpec, Scope, delivery},
};

fn alloc() -> LaunchAlloc {
    blaze_own::testing::heap().shared()
}

fn text(raw: &str, alloc: LaunchAlloc) -> blaze_own::String<LaunchAlloc> {
    blaze_own::String::from_str_in(raw, alloc)
}

fn dir(path: &str, write: bool, alloc: LaunchAlloc) -> Scope<LaunchAlloc> {
    Scope::Dir { path: text(path, alloc), write }
}

fn net(at: &str, listen: bool, alloc: LaunchAlloc) -> Scope<LaunchAlloc> {
    Scope::Net { at: text(at, alloc), listen }
}

/// File descriptor 3's bytes for a run spec granting `grants` (`(name, scope)` each).
fn spec(grants: &[(&str, Scope<LaunchAlloc>)], alloc: LaunchAlloc) -> blaze_own::Vec<u8, LaunchAlloc> {
    let mut spec = RunSpec::defaults(blaze_own::Vec::new_in(alloc), blaze_own::Vec::new_in(alloc), text("/work", alloc), alloc);
    for (name, scope) in grants {
        let name = GrantName::new(name, alloc).expect("a grant's name (a member's `p.m` too)");
        spec.put_grant(Granted { name, scope: scope.clone(), provider: None, layers: None });
    }
    delivery(&spec, alloc)
}

/// `body` launched as `manifest`'s program under the spec `fd3`: its status and words.
fn launch<R: crate::entry::Outcome>(
    manifest: &'static Manifest,
    fd3: &[u8],
    body: impl FnOnce(&mut crate::grant::Given<'_>) -> Result<R, crate::grant::Refused>,
) -> (Status, std::string::String) {
    let mut said = std::string::String::new();
    let status = launch_in(manifest, Some(fd3), Io::sim(1), &mut said, body);
    (status, said)
}

static MAYBE: Manifest = Manifest::new(&[Param::of::<Maybe<Dir<ReadWrite>>>("cache")]);

fn takes_maybe(_cx: &Cx, cache: Maybe<Dir<ReadWrite>>) -> Status {
    match cache.get() {
        Some(_) => Status(3),
        None => Status(4),
    }
}

/// An absent grant is never a refusal: no grant of `cache` starts `main` with `None`; a
/// read-write grant gives `Some`.
/// Expected: `test grant::compose::tests::an_absent_maybe_is_none_never_a_refusal ... ok`.
#[test]
fn an_absent_maybe_is_none_never_a_refusal() {
    let a = alloc();
    let (status, said) = launch(&MAYBE, &spec(&[], a), crate::launch_body!(takes_maybe; cache: Maybe<Dir<ReadWrite>>));
    assert_eq!((status, said.as_str()), (Status(4), ""));
    let (status, said) = launch(&MAYBE, &spec(&[("cache", dir("cache", true, a))], a), crate::launch_body!(takes_maybe; cache: Maybe<Dir<ReadWrite>>));
    assert_eq!((status, said.as_str()), (Status(3), ""));
}

/// A wrong grant always is a refusal: `cache` granted read only (it writes) or as an
/// endpoint refuses the start, as it would for a plain `Dir<ReadWrite>`. Control: the
/// absent case above starts.
/// Expected: `test grant::compose::tests::a_wrong_maybe_grant_is_always_a_refusal ... ok`.
#[test]
fn a_wrong_maybe_grant_is_always_a_refusal() {
    let a = alloc();
    let (status, said) = launch(&MAYBE, &spec(&[("cache", dir("cache", false, a))], a), crate::launch_body!(takes_maybe; cache: Maybe<Dir<ReadWrite>>));
    assert_eq!(status, Status::REFUSED, "{said}");
    assert!(said.contains("- cache (Dir<ReadWrite>): granted read only, and it writes"), "{said}");
    let (status, said) = launch(&MAYBE, &spec(&[("cache", net("example.com:443", false, a))], a), crate::launch_body!(takes_maybe; cache: Maybe<Dir<ReadWrite>>));
    assert_eq!(status, Status::REFUSED, "{said}");
}

static WRITE_ELSE_READ: Manifest = Manifest::new(&[Param::of::<OneOf<(Dir<ReadWrite>, Dir<ReadOnly>)>>("store")]);

fn takes_store(_cx: &Cx, store: OneOf<(Dir<ReadWrite>, Dir<ReadOnly>)>) -> Status {
    match store.chosen() {
        Chosen2::First(_) => Status(5),
        Chosen2::Second(_) => Status(6),
    }
}

/// "Write if granted, else read" (`Dir<ReadWrite>` first) is accepted and chooses by the
/// grant: read-write gives the first, read-only the second (the first passed over as it would
/// widen); no grant refuses naming both alternatives.
/// Expected: `test grant::compose::tests::one_of_takes_the_first_alternative_granted ... ok`.
#[test]
fn one_of_takes_the_first_alternative_granted() {
    let a = alloc();
    let body = || crate::launch_body!(takes_store; store: OneOf<(Dir<ReadWrite>, Dir<ReadOnly>)>);
    let (status, said) = launch(&WRITE_ELSE_READ, &spec(&[("store", dir("store", true, a))], a), body());
    assert_eq!((status, said.as_str()), (Status(5), ""));
    let (status, said) = launch(&WRITE_ELSE_READ, &spec(&[("store", dir("store", false, a))], a), body());
    assert_eq!((status, said.as_str()), (Status(6), ""));
    let (status, said) = launch(&WRITE_ELSE_READ, &spec(&[], a), body());
    assert_eq!(status, Status::REFUSED, "{said}");
    assert!(said.contains("none of its alternatives is granted"), "{said}");
    assert!(said.contains("`--grant store=dir:<folder>`") && said.contains("`--grant store=read:<folder>`"), "every alternative's fix: {said}");
}

static FOLDER_OR_ENDPOINT: Manifest = Manifest::new(&[Param::of::<OneOf<(Dir<ReadWrite>, Connect)>>("store")]);

fn takes_folder_or_endpoint(_cx: &Cx, store: OneOf<(Dir<ReadWrite>, Connect)>) -> Status {
    match store.into_chosen() {
        Chosen2::First(_) => Status(7),
        Chosen2::Second(_) => Status(8),
    }
}

/// Alternatives of two kinds: an endpoint grant passes over the folder and takes the second.
/// Expected: `test grant::compose::tests::one_of_two_kinds_is_chosen_by_the_grant_s_kind ... ok`.
#[test]
fn one_of_two_kinds_is_chosen_by_the_grant_s_kind() {
    let a = alloc();
    let body = || crate::launch_body!(takes_folder_or_endpoint; store: OneOf<(Dir<ReadWrite>, Connect)>);
    let (status, said) = launch(&FOLDER_OR_ENDPOINT, &spec(&[("store", net("example.com:443", false, a))], a), body());
    assert_eq!((status, said.as_str()), (Status(8), ""));
    let (status, said) = launch(&FOLDER_OR_ENDPOINT, &spec(&[("store", dir("store", true, a))], a), body());
    assert_eq!((status, said.as_str()), (Status(7), ""));
}

static MANY: Manifest = Manifest::new(&[Param::of::<Many<Dir<ReadOnly>>>("inputs")]);

/// `Status(10 * count + 1 if the order is a, b)`.
fn takes_many(_cx: &Cx, inputs: Many<Dir<ReadOnly>>) -> Status {
    let mut names = inputs.iter().map(|(name, _)| name);
    let ordered = matches!((names.next(), names.next(), names.next()), (Some("a"), Some("b"), None));
    Status(10 * inputs.len() as u8 + u8::from(ordered))
}

/// Every `inputs.<member>` grant, in name order (granted b then a, handed a then b); zero is an
/// empty `Many`, not a refusal; a member granted the wrong kind refuses, naming the member.
/// Expected: `test grant::compose::tests::many_takes_every_member_in_name_order ... ok`.
#[test]
fn many_takes_every_member_in_name_order() {
    let a = alloc();
    let body = || crate::launch_body!(takes_many; inputs: Many<Dir<ReadOnly>>);
    let fd3 = spec(&[("inputs.b", dir("b", false, a)), ("inputs.a", dir("a", false, a))], a);
    let (status, said) = launch(&MANY, &fd3, body());
    assert_eq!((status, said.as_str()), (Status(21), ""));
    let (status, said) = launch(&MANY, &spec(&[], a), body());
    assert_eq!((status, said.as_str()), (Status(0), ""));
    let (status, said) = launch(&MANY, &spec(&[("inputs.a", net("example.com:443", false, a))], a), body());
    assert_eq!(status, Status::REFUSED, "{said}");
    assert!(said.contains("`inputs.a`: granted a `net`"), "{said}");
    // Control: a grant under another prefix belongs to no parameter, and says so.
    let (status, said) = launch(&MANY, &spec(&[("other.a", dir("a", false, a))], a), body());
    assert_eq!(status, Status::REFUSED, "{said}");
    assert!(said.contains("- other.a (dir): granted, and `main` has no parameter"), "{said}");
}

static LISTENS: Manifest = Manifest::new(&[Param::of::<Listen>("feed")]);

fn takes_listen(_cx: &Cx, feed: Listen) -> Status {
    let _ = feed;
    Status(9)
}

/// A `Listen` takes a listening grant and refuses one that connects.
/// Expected: `test grant::compose::tests::a_direction_in_the_type_refuses_the_other ... ok`.
#[test]
fn a_direction_in_the_type_refuses_the_other() {
    let a = alloc();
    let (status, said) = launch(&LISTENS, &spec(&[("feed", net("127.0.0.1:8080", true, a))], a), crate::launch_body!(takes_listen; feed: Listen));
    assert_eq!((status, said.as_str()), (Status(9), ""));
    let (status, said) = launch(&LISTENS, &spec(&[("feed", net("127.0.0.1:8080", false, a))], a), crate::launch_body!(takes_listen; feed: Listen));
    assert_eq!(status, Status::REFUSED, "{said}");
    assert!(said.contains("granted a `net (connect)`"), "{said}");
}

/// Each form's manifest entry, and a plain one BYTE-EQUAL to the form written before forms
/// existed (grant.rs `Manifest::write`'s doc).
/// Expected: `test grant::compose::tests::each_form_is_in_the_manifest_and_plain_ones_are_unchanged ... ok`.
#[test]
fn each_form_is_in_the_manifest_and_plain_ones_are_unchanged() {
    let a = alloc();
    static PLAIN: Manifest = Manifest::new(&[Param::of::<Dir<ReadOnly>>("data")]);
    assert_eq!(PLAIN.write(a).as_str(), r#"{"params":[{"fills_itself":false,"kind":"dir","name":"data","write":false}],"version":1}"#);
    assert_eq!(PLAIN.params[0], Param::new("data", crate::grant::Kind::Dir { write: false }), "`of` of a plain type is `new`");
    assert_eq!(
        MAYBE.write(a).as_str(),
        r#"{"params":[{"fills_itself":false,"form":"optional","kind":"dir","name":"cache","write":true}],"version":1}"#
    );
    assert_eq!(
        WRITE_ELSE_READ.write(a).as_str(),
        r#"{"params":[{"alternatives":[{"kind":"dir","write":true},{"kind":"dir","write":false}],"fills_itself":false,"form":"one_of","kind":"dir","name":"store","write":true}],"version":1}"#
    );
    assert_eq!(
        MANY.write(a).as_str(),
        r#"{"params":[{"fills_itself":false,"form":"many","kind":"dir","min":0,"name":"inputs","write":false}],"version":1}"#
    );
    assert_eq!(
        LISTENS.write(a).as_str(),
        r#"{"params":[{"direction":"listen","fills_itself":false,"kind":"net","name":"feed"}],"version":1}"#
    );
}

/// A member's name is a grant's name; a parameter's is not allowed a dot.
/// Expected: `test grant::compose::tests::a_member_name_has_one_dot ... ok`.
#[test]
fn a_member_name_has_one_dot() {
    assert!(super::is_grant_name("inputs.a") && super::is_grant_name("inputs"));
    assert!(!super::is_grant_name("inputs.a.b") && !super::is_grant_name(".a") && !super::is_grant_name("inputs."));
    assert!(!crate::spec::is_name("inputs.a"));
    assert_eq!(super::member_of("inputs.a", "inputs"), Some("a"));
    assert_eq!(super::member_of("inputsx.a", "inputs"), None);
}
