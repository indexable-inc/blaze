//! What the leaves allow and what the transfers give, value by value (design sections 1, 5, 6).

use blaze_alloc_count::assert_at_most;
use intern::Symbol;

use crate::{
    Abs, Effects, Root,
    models::{Applied, Leaf, Transfer, Transferred, apply_leaf, apply_transfer, tag},
};

fn c(text: &str) -> Abs {
    Abs::Const(Symbol::intern(text))
}

fn env(name: &str) -> Abs {
    Abs::Env(Symbol::intern(name))
}

fn done(effects: Effects) -> Applied {
    Applied::Done { effects, env: None, ret: Abs::Unknown }
}

#[track_caller]
fn refused(applied: Applied, says: &str) {
    match applied {
        Applied::Refused(what) => assert!(what.contains(says), "`{what}` should say `{says}`"),
        other => panic!("{other:?} should be refused, saying `{says}`"),
    }
}

const LEAVES: [Leaf; 12] = [
    Leaf::EnvVar,
    Leaf::Print,
    Leaf::ReadFile,
    Leaf::WriteFile,
    Leaf::Copy,
    Leaf::Probe,
    Leaf::Handle,
    Leaf::RawHandle,
    Leaf::Exit,
    Leaf::Panic,
    Leaf::Alloc,
    Leaf::ErrorText,
];

#[test]
fn env_var_reads_only_what_cargo_gives() {
    for name in ["OUT_DIR", "TARGET", "CARGO_MANIFEST_DIR", "CARGO_PKG_NAME", "CARGO_CFG_UNIX"] {
        let name = Symbol::intern(name);
        assert_eq!(
            apply_leaf(Leaf::EnvVar, &[Abs::Const(name)]),
            Applied::Done { effects: Effects::NONE, env: Some(name), ret: Abs::Env(name) },
        );
    }
    for name in ["PATH", "RUSTC", "RUSTC_WRAPPER", "CARGO_MAKEFLAGS", "DEP_Z_INCLUDE", "CARGO", "HOME"] {
        refused(apply_leaf(Leaf::EnvVar, &[c(name)]), &format!("`{name}`"));
    }
    assert_eq!(apply_leaf(Leaf::EnvVar, &[Abs::Param(0)]), Applied::Deferred);
    for value in [Abs::Unknown, Abs::Unset, env("CARGO_PKG_NAME"), Abs::Root(Root::OutDir)] {
        refused(apply_leaf(Leaf::EnvVar, &[value]), "a variable named by a value not known");
    }
    refused(apply_leaf(Leaf::EnvVar, &[]), "a variable named by a value not known");
}

#[test]
fn reads_and_probes_are_of_package_paths() {
    for path in [c("Cargo.toml"), c("src/x.rs"), env("CARGO_MANIFEST_DIR"), Abs::Root(Root::Package)] {
        assert_eq!(apply_leaf(Leaf::ReadFile, &[path]), done(Effects::READ));
        assert_eq!(apply_leaf(Leaf::Probe, &[path]), done(Effects::PROBE));
    }
    refused(apply_leaf(Leaf::ReadFile, &[c("../x")]), "reads `../x`, not a path inside its package");
    refused(apply_leaf(Leaf::ReadFile, &[c("/etc/x")]), "reads `/etc/x`, not a path inside its package");
    refused(apply_leaf(Leaf::ReadFile, &[c("C:x")]), "`C:x`");
    refused(apply_leaf(Leaf::ReadFile, &[Abs::Root(Root::OutDir)]), "a path in its OUT_DIR");
    refused(apply_leaf(Leaf::ReadFile, &[env("OUT_DIR")]), "a path in its OUT_DIR");
    refused(apply_leaf(Leaf::ReadFile, &[env("TARGET")]), "the path the variable `TARGET` gives");
    refused(apply_leaf(Leaf::ReadFile, &[Abs::Unknown]), "a path not known");
    refused(apply_leaf(Leaf::Probe, &[Abs::Unset]), "a path not known");
    assert_eq!(apply_leaf(Leaf::ReadFile, &[Abs::Param(2)]), Applied::Deferred);
}

#[test]
fn writes_are_of_out_dir_paths() {
    for path in [env("OUT_DIR"), Abs::Root(Root::OutDir)] {
        assert_eq!(apply_leaf(Leaf::WriteFile, &[path]), done(Effects::WRITE));
    }
    refused(apply_leaf(Leaf::WriteFile, &[c("src/gen.rs")]), "writes `src/gen.rs`, a path in its package");
    refused(apply_leaf(Leaf::WriteFile, &[Abs::Root(Root::Package)]), "a path in its package");
    refused(apply_leaf(Leaf::WriteFile, &[env("CARGO_MANIFEST_DIR")]), "a path in its package");
    refused(apply_leaf(Leaf::WriteFile, &[c("/tmp/x")]), "`/tmp/x`");
    assert_eq!(apply_leaf(Leaf::WriteFile, &[Abs::Param(0)]), Applied::Deferred);
}

#[test]
fn a_copy_reads_the_package_into_out_dir() {
    let both = Effects::READ.union(Effects::WRITE);
    assert_eq!(apply_leaf(Leaf::Copy, &[c("data.bin"), Abs::Root(Root::OutDir)]), done(both));
    refused(apply_leaf(Leaf::Copy, &[Abs::Root(Root::OutDir), Abs::Root(Root::OutDir)]), "reads");
    refused(apply_leaf(Leaf::Copy, &[c("data.bin"), c("data.bin")]), "writes");
    assert_eq!(apply_leaf(Leaf::Copy, &[Abs::Param(0), Abs::Root(Root::OutDir)]), Applied::Deferred);
    assert_eq!(apply_leaf(Leaf::Copy, &[c("a"), Abs::Param(1)]), Applied::Deferred);
    // A refusal wins over a parameter that decides nothing it could save.
    refused(apply_leaf(Leaf::Copy, &[Abs::Param(0), c("/etc/x")]), "writes `/etc/x`");
    refused(apply_leaf(Leaf::Copy, &[c("../x"), Abs::Param(1)]), "reads `../x`");
}

#[test]
fn the_rest_take_any_arguments() {
    let any = [Abs::Unknown, Abs::Unset, c("/etc/passwd"), Abs::Param(3)];
    assert_eq!(apply_leaf(Leaf::Print, &any), done(Effects::PRINT));
    assert_eq!(apply_leaf(Leaf::Exit, &any), done(Effects::EXIT));
    for leaf in [Leaf::Handle, Leaf::Panic, Leaf::Alloc, Leaf::ErrorText] {
        assert_eq!(apply_leaf(leaf, &any), done(Effects::NONE));
        assert_eq!(apply_leaf(leaf, &[]), done(Effects::NONE));
    }
    refused(apply_leaf(Leaf::RawHandle, &[c("3")]), "makes a file handle of a number");
}

#[test]
fn leaves_are_monotone() {
    let values = [
        Abs::Unset,
        Abs::Unknown,
        Abs::Param(0),
        Abs::Param(1),
        Abs::Under(0),
        Abs::Under(1),
        c("OUT_DIR"),
        c("PATH"),
        c("x.rs"),
        c("../x"),
        env("OUT_DIR"),
        env("CARGO_MANIFEST_DIR"),
        env("TARGET"),
        Abs::Root(Root::Package),
        Abs::Root(Root::OutDir),
    ];
    for leaf in LEAVES {
        for &a in &values {
            for &b in &values {
                let given = apply_leaf(leaf, &[a, b]);
                for less in [[Abs::Unknown, b], [a, Abs::Unknown], [Abs::Unknown, Abs::Unknown]] {
                    let less_known = apply_leaf(leaf, &less);
                    if matches!(given, Applied::Refused(_)) {
                        assert!(
                            matches!(less_known, Applied::Refused(_)),
                            "{leaf:?} of {a:?}, {b:?} is refused, of {less:?} {less_known:?}"
                        );
                    }
                    if !less.iter().any(|it| matches!(it, Abs::Param(_) | Abs::Under(_))) {
                        assert_ne!(less_known, Applied::Deferred, "{leaf:?} of {less:?} names no parameter");
                    }
                }
            }
        }
    }
}

#[test]
fn a_join_keeps_or_replaces_the_root() {
    let join = |base, rel| apply_transfer(Transfer::Join { base: 0, rel: 1 }, &[base, rel]).ret;
    assert_eq!(join(env("OUT_DIR"), c("x.rs")), Abs::Root(Root::OutDir));
    assert_eq!(join(Abs::Root(Root::OutDir), c("gen/x.rs")), Abs::Root(Root::OutDir));
    assert_eq!(join(env("CARGO_MANIFEST_DIR"), c("Cargo.toml")), Abs::Root(Root::Package));
    assert_eq!(join(c("src"), c("lib.rs")), Abs::Root(Root::Package));
    assert_eq!(join(env("OUT_DIR"), c("../x")), Abs::Unknown);
    assert_eq!(join(env("OUT_DIR"), c("/etc/x")), Abs::Unknown);
    assert_eq!(join(c("/etc"), c("x")), Abs::Unknown);
    assert_eq!(join(env("TARGET"), c("x")), Abs::Unknown);
    // An absolute path replaces the base.
    assert_eq!(join(c("/etc"), env("OUT_DIR")), Abs::Root(Root::OutDir));
    assert_eq!(join(Abs::Unknown, env("CARGO_MANIFEST_DIR")), Abs::Root(Root::Package));
    assert_eq!(join(Abs::Root(Root::Package), Abs::Root(Root::OutDir)), Abs::Root(Root::OutDir));
    // A package path may be relative: under the package it stays there, under OUT_DIR it is not
    // known.
    assert_eq!(join(Abs::Root(Root::Package), Abs::Root(Root::Package)), Abs::Root(Root::Package));
    assert_eq!(join(Abs::Root(Root::OutDir), Abs::Root(Root::Package)), Abs::Unknown);
    for unknown in [Abs::Unknown, Abs::Unset] {
        assert_eq!(join(unknown, c("x.rs")), Abs::Unknown);
    }
    for unknown in [Abs::Unknown, Abs::Unset, Abs::Param(0), Abs::Under(0)] {
        assert_eq!(join(env("OUT_DIR"), unknown), Abs::Unknown);
    }
    // A parameter joined with safe relative constants stays a path under it, for its caller.
    assert_eq!(join(Abs::Param(1), c("x.rs")), Abs::Under(1));
    assert_eq!(join(Abs::Under(1), c("gen/y.rs")), Abs::Under(1));
    assert_eq!(join(Abs::Param(1), c("../x")), Abs::Unknown);
    assert_eq!(join(Abs::Under(1), c("/etc/x")), Abs::Unknown);
    assert_eq!(join(Abs::Param(1), env("OUT_DIR")), Abs::Root(Root::OutDir));
}

#[test]
fn a_path_under_a_parameter_defers_and_substitutes() {
    for leaf in [Leaf::ReadFile, Leaf::WriteFile, Leaf::Probe] {
        assert_eq!(apply_leaf(leaf, &[Abs::Under(0)]), Applied::Deferred);
    }
    assert_eq!(apply_leaf(Leaf::Copy, &[c("a"), Abs::Under(1)]), Applied::Deferred);
    refused(apply_leaf(Leaf::EnvVar, &[Abs::Under(0)]), "a variable named by a value not known");
    let joined = apply_transfer(Transfer::Join { base: 0, rel: 1 }, &[Abs::Param(0), c("x.rs")]).ret;
    assert_eq!(joined.substitute(&[env("OUT_DIR")]), Abs::Root(Root::OutDir));
    assert_eq!(joined.substitute(&[c("src")]), Abs::Root(Root::Package));
    assert_eq!(joined.substitute(&[c("/etc")]), Abs::Unknown);
    assert_eq!(joined.substitute(&[Abs::Param(2)]), Abs::Under(2));
    let written = joined.substitute(&[env("OUT_DIR")]);
    assert_eq!(apply_leaf(Leaf::WriteFile, &[written]), done(Effects::WRITE));
}

#[test]
fn a_push_writes_the_join_back() {
    let push = Transfer::Push { target: 0, rel: 1 };
    assert_eq!(
        apply_transfer(push, &[env("OUT_DIR"), c("x.rs")]),
        Transferred { ret: Abs::Unknown, update: Some((0, Abs::Root(Root::OutDir))) },
    );
    assert_eq!(
        apply_transfer(push, &[env("OUT_DIR"), c("../x.rs")]),
        Transferred { ret: Abs::Unknown, update: Some((0, Abs::Unknown)) },
    );
    let push = Transfer::Push { target: 1, rel: 0 };
    assert_eq!(
        apply_transfer(push, &[c("a"), env("CARGO_MANIFEST_DIR")]),
        Transferred { ret: Abs::Unknown, update: Some((1, Abs::Root(Root::Package))) },
    );
    assert_eq!(
        apply_transfer(Transfer::Push { target: 0, rel: 1 }, &[Abs::Param(0), c("x.rs")]),
        Transferred { ret: Abs::Unknown, update: Some((0, Abs::Under(0))) },
    );
}

#[test]
fn same_gives_the_argument() {
    for value in [c("x"), env("OUT_DIR"), Abs::Root(Root::Package), Abs::Param(1), Abs::Unknown] {
        assert_eq!(
            apply_transfer(Transfer::Same { arg: 1 }, &[Abs::Unknown, value]),
            Transferred { ret: value, update: None },
        );
    }
    assert_eq!(apply_transfer(Transfer::Same { arg: 0 }, &[Abs::Unset]).ret, Abs::Unknown);
    assert_eq!(apply_transfer(Transfer::Same { arg: 4 }, &[c("x")]).ret, Abs::Unknown);
}

#[test]
fn tags_are_distinct_and_fixed() {
    let tags = LEAVES.map(tag);
    for (i, a) in tags.iter().enumerate() {
        assert!(tags[i + 1..].iter().all(|b| a != b), "{:?} shares its tag", LEAVES[i]);
        assert_ne!(*a, 0);
    }
    assert_eq!(
        (tag(Leaf::EnvVar), tag(Leaf::Alloc), tag(Leaf::RawHandle), tag(Leaf::ErrorText)),
        (1, 10, 11, 12)
    );
}

#[test]
fn applying_allocates_only_a_refusal() {
    let out = env("OUT_DIR");
    let rel = c("x.rs");
    let name = c("OUT_DIR");
    assert_at_most(0, "leaves and transfers that allow", || {
        let mut all = 0usize;
        for _ in 0..64 {
            all += matches!(apply_leaf(Leaf::EnvVar, &[name]), Applied::Done { .. }) as usize;
            all += matches!(apply_leaf(Leaf::WriteFile, &[out]), Applied::Done { .. }) as usize;
            all += matches!(apply_leaf(Leaf::ReadFile, &[Abs::Param(0)]), Applied::Deferred) as usize;
            all += (apply_transfer(Transfer::Join { base: 0, rel: 1 }, &[out, rel]).ret
                == Abs::Root(Root::OutDir)) as usize;
            all += apply_transfer(Transfer::Push { target: 0, rel: 1 }, &[out, rel]).update.is_some() as usize;
        }
        assert_eq!(all, 64 * 5);
    });
}
