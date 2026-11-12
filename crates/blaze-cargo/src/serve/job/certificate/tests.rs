//! The certificate against a walk: a model checkout of `n` packages in a chain (package `i`
//! depends on `i - 1`), each a directory of the view with one unit, one variable read, one
//! probe outside the view and one output. The oracle is the walk: [`World::walk`] is what a
//! fresh build of the world would write, and a certificate is given only when it is that.
//!
//! The invariant: a certificate is given only when a fresh walk would produce the same
//! outputs and say the same; and it fails only for one of the six things that differ
//! (certificate.md, "Why a certificate fails"), naming which.

use std::collections::BTreeMap;

use super::*;

fn id(domain: &'static str, bytes: &[u8]) -> ContentId {
    ContentId::of(domain, bytes)
}

#[derive(Clone, Debug)]
struct World {
    /// Where the checkout is: in no key.
    dir: String,
    /// Each package's source, by content.
    sources: Vec<u8>,
    /// A file of the checkout no unit reads.
    readme: u8,
    /// The variable a build script reads, and one nothing reads.
    made: Option<u8>,
    shlvl: u8,
    /// The path a build script looked for outside the view: there or not.
    probe: Option<u8>,
    /// The outputs in place, by path, each with its stamp.
    placed: BTreeMap<String, Stamp>,
    profile: &'static str,
    blaze: &'static str,
    traced: bool,
}

impl World {
    fn new(n: usize) -> World {
        World {
            dir: "/home/a/checkout".to_owned(),
            sources: vec![0; n],
            readme: 0,
            made: Some(1),
            shlvl: 0,
            probe: None,
            placed: BTreeMap::new(),
            profile: "dev",
            blaze: "blaze 1",
            traced: false,
        }
    }

    fn group(&self, at: usize) -> ContentId {
        id("test package directory", &[at as u8, self.sources[at]])
    }

    fn root(&self) -> ContentId {
        let mut bytes = self.sources.clone();
        bytes.push(self.readme);
        id("test view root", &bytes)
    }

    fn shape(&self) -> ContentId {
        let config = format!("build --profile {} --manifest-path {}/Cargo.toml", self.profile, self.dir);
        id("test shape", relocated(&config, &self.dir).as_bytes())
    }

    fn stamp(&self, at: usize) -> Stamp {
        // An output is a function of its package's source and every one before it.
        let sum: u64 = self.sources[..=at].iter().map(|it| u64::from(*it)).sum();
        Stamp { len: 100 + sum, changed_ns: 7 }
    }

    /// What a fresh walk of the world writes: its certificate, the outputs placed.
    fn walk(&mut self) -> Certificate {
        let n = self.sources.len();
        self.placed = (0..n).map(|at| (format!("debug/p{at}"), self.stamp(at))).collect();
        Certificate {
            blaze: self.blaze.to_owned(),
            shape: self.shape(),
            config: id("test config", self.profile.as_bytes()),
            request: id("test request", b""),
            drawn: id("test drawn", b""),
            diagnosed: false,
            root: self.root(),
            groups: (0..n)
                .map(|at| Group {
                    path: format!("p{at}"),
                    id: Some(self.group(at)),
                    readers: vec![at as u32],
                })
                .collect(),
            env: vec![("MADE".to_owned(), self.made.map(|it| id("test env", &[it])))],
            outside: vec![("/usr/include/probe.h".to_owned(), self.probe.map(|it| [it; 32]))],
            outputs: (0..n)
                .map(|at| Output {
                    base: 0,
                    path: format!("debug/p{at}"),
                    content: id("test output", &self.stamp(at).len.to_le_bytes()),
                    stamp: self.stamp(at),
                })
                .collect(),
            units: (0..n).map(|at| id("test unit", &[at as u8, self.sources[at]])).collect(),
            said: self.sources.clone(),
        }
    }

    fn ask(&self, certificate: &Certificate) -> Result<(), Failed> {
        let env = |name: &str| match name {
            "MADE" => self.made.map(|it| id("test env", &[it])),
            "SHLVL" => Some(id("test env", &[self.shlvl])),
            _ => None,
        };
        let now = Now {
            blaze: self.blaze,
            shape: self.shape(),
            config: id("test config", self.profile.as_bytes()),
            request: id("test request", b""),
            drawn: id("test drawn", b""),
            root: self.root(),
            env: &env,
            traced: self.traced,
        };
        certificate.holds(&now, self)
    }

    /// The oracle: a certificate given is the one a fresh walk of this world writes, and the
    /// outputs in place are that walk's.
    fn given_rightly(&self, certificate: &Certificate) -> bool {
        let mut fresh = self.clone();
        let walked = fresh.walk();
        walked == *certificate && fresh.placed == self.placed
    }
}

impl Looks for World {
    fn node(&self, path: &str) -> Option<ContentId> {
        let at: usize = path.strip_prefix('p')?.parse().ok()?;
        (at < self.sources.len()).then(|| self.group(at))
    }

    fn outside(&self, _: &str) -> Option<[u8; 32]> {
        self.probe.map(|it| [it; 32])
    }

    fn output(&self, _: u8, path: &str) -> Option<Stamp> {
        self.placed.get(path).copied()
    }
}

/// Each way a world moves, with what the failure must name.
fn moves() -> [(&'static str, fn(&mut World), fn(&Failed) -> bool, Plant); 5] {
    [
        (
            "(1) a source's content",
            |w| w.sources[1] += 1,
            |f| matches!(f, Failed::Group { path, readers } if path == "p1" && readers == &[1]),
            Plant::IgnoresGroups,
        ),
        (
            "(2) a probe that became present",
            |w| w.probe = Some(3),
            |f| matches!(f, Failed::Outside { path, appeared: true } if path.ends_with("probe.h")),
            Plant::IgnoresProbes,
        ),
        (
            "(3) a variable a build script read",
            |w| w.made = Some(2),
            |f| matches!(f, Failed::Env(name) if name == "MADE"),
            Plant::IgnoresEnv,
        ),
        (
            "(4) the request's shape",
            |w| w.profile = "release",
            |f| matches!(f, Failed::Request(_)),
            Plant::IgnoresRequest,
        ),
        (
            "(6) an output deleted",
            |w| _ = w.placed.remove("debug/p0"),
            |f| matches!(f, Failed::Output { path, .. } if path == "debug/p0"),
            Plant::IgnoresOutputs,
        ),
    ]
}

/// The invariant: a certificate fails for each of the things that differ, naming it, and a
/// check with that compare planted out gives it wrongly (the control), which the oracle says.
#[test]
fn a_certificate_fails_only_for_what_differs_and_names_it() {
    for (what, change, names, plant) in moves() {
        let mut world = World::new(3);
        let certificate = world.walk();
        assert_eq!(world.ask(&certificate), Ok(()), "{what}: a repeated request is answered");
        assert!(world.given_rightly(&certificate));
        change(&mut world);
        let failed = world.ask(&certificate).expect_err(what);
        assert!(names(&failed), "{what}: named {failed:?}");
        assert!(!failed.input().is_empty());
        // Control: the planted bug gives the certificate, and the oracle catches it.
        PLANT.set(plant);
        let given = world.ask(&certificate);
        PLANT.set(Plant::None);
        assert_eq!(given, Ok(()), "{what}: the planted check gives it");
        assert!(!world.given_rightly(&certificate), "{what}: the oracle catches the planted bug");
    }
}

/// (5): another blaze's certificate is not given, and its key is another.
#[test]
fn another_blaze_is_not_answered() {
    let mut world = World::new(2);
    let certificate = world.walk();
    world.blaze = "blaze 2";
    assert_eq!(world.ask(&certificate), Err(Failed::Blaze));
    assert_ne!(
        key(&world.shape(), &world.root(), "blaze 1"),
        key(&world.shape(), &world.root(), "blaze 2")
    );
}

/// Nothing else makes it fail: a restart (the certificate read back from its bytes), a file
/// of the checkout no unit read, a variable no step read, another checkout's directory.
/// Control for each: the same change to something that IS read fails (the test above).
#[test]
fn nothing_else_fails_a_certificate() {
    let mut world = World::new(3);
    let written = world.walk().encode();
    // A restart: nothing in memory, the record's bytes alone.
    let restored = Certificate::decode(&written).expect("its own layout");
    assert_eq!(world.ask(&restored), Ok(()));
    // A file no unit read: the root is another, every group is as it was.
    world.readme += 1;
    assert_ne!(world.root(), restored.root);
    assert_eq!(world.ask(&restored), Ok(()));
    assert!(world.given_rightly(&Certificate { root: world.root(), ..restored.clone() }));
    // A variable that is another at every command.
    world.shlvl += 1;
    assert_eq!(world.ask(&restored), Ok(()));
    // A second checkout of the same content, elsewhere: the same key, so it finds the
    // record; its outputs are not placed yet, which is what the failure names, and no unit.
    let mut second = World::new(3);
    second.dir = "/tmp/other/place".to_owned();
    assert_eq!(second.shape(), World::new(3).shape());
    let first = World::new(3);
    assert_eq!(
        key(&second.shape(), &second.root(), second.blaze),
        key(&first.shape(), &first.root(), first.blaze)
    );
    let found = second.ask(&restored).expect_err("no output placed yet");
    assert!(matches!(found, Failed::Output { .. }));
    assert_eq!(found.readers(), None);
    // The slot is the checkout's own.
    assert_ne!(slot(&second.shape(), &second.dir), slot(&first.shape(), &first.dir));
}

/// The walk after an edit starts at the readers of the group that moved: it visits at most
/// those and their dependents, at two graph sizes; a walk from the roots would visit all.
#[test]
fn the_walk_after_an_edit_starts_at_the_readers() {
    for n in [4_usize, 64] {
        let mut world = World::new(n);
        let certificate = world.walk();
        let edited = n - 2;
        world.sources[edited] += 1;
        let failed = world.ask(&certificate).expect_err("an edit");
        // Package `i + 1` depends on `i`.
        let dependents = |unit: u32| if (unit as usize) + 1 < n { vec![unit + 1] } else { vec![] };
        let visited = certificate.walk_from(&failed, &dependents).expect("a group names its readers");
        assert_eq!(visited, vec![edited as u32, edited as u32 + 1], "{n} packages");
        assert!(visited.len() < n);
    }
}

/// Persisted state round-trips (rule 26): the same bytes again, a previous layout refused by
/// its name, a cut record refused at its field, never read as an empty certificate.
#[test]
fn a_certificate_round_trips_and_refuses_other_layouts() {
    let mut world = World::new(3);
    world.probe = Some(9);
    let certificate = world.walk();
    let bytes = certificate.encode();
    let read = Certificate::decode(&bytes).expect("its own layout");
    assert_eq!(read, certificate);
    assert_eq!(read.encode(), bytes);
    let mut previous = bytes.clone();
    previous[MAGIC.len()..MAGIC.len() + 4].copy_from_slice(&0_u32.to_le_bytes());
    let refused = Certificate::decode(&previous).expect_err("layout 0");
    assert!(matches!(refused, Undecoded::Previous(0, _)));
    assert!(refused.to_string().contains("the draft of 2026-11-12"));
    let mut unknown = bytes.clone();
    unknown[MAGIC.len()..MAGIC.len() + 4].copy_from_slice(&99_u32.to_le_bytes());
    assert_eq!(Certificate::decode(&unknown), Err(Undecoded::Unknown(99)));
    assert_eq!(Certificate::decode(b"something else"), Err(Undecoded::NotOne));
    for cut in MAGIC.len() + 4..bytes.len() {
        assert!(
            matches!(Certificate::decode(&bytes[..cut]), Err(Undecoded::Malformed(_))),
            "cut at {cut}"
        );
    }
    let mut longer = bytes;
    longer.push(0);
    assert_eq!(Certificate::decode(&longer), Err(Undecoded::Malformed("the end")));
}

#[test]
fn the_checkout_directory_is_relocated_only_as_a_whole_path() {
    assert_eq!(relocated("/a/b/Cargo.toml", "/a/b"), format!("{ROOT}/Cargo.toml"));
    assert_eq!(relocated("--target-dir=/a/b/target", "/a/b"), format!("--target-dir={ROOT}/target"));
    assert_eq!(relocated("/a/bc/x", "/a/b"), "/a/bc/x");
    assert_eq!(relocated("/x/a/b/y", "/a/b"), "/x/a/b/y");
    assert_eq!(relocated("/a/b", "/a/b"), ROOT);
}
