use std::collections::BTreeMap;

use blaze_cas::Key;

use super::*;
use crate::{
    Given, RunKey, RunKind,
    index::{self, Found, Shape},
    trace::{DIRECTORY, listing},
};

fn checkout(home: &str) -> RootMap {
    RootMap::default()
        .with(Role::Package, Path::new(&format!("{home}/ws/p")))
        .with(Role::OutDir, Path::new(&format!("{home}/ws/target/debug/build/p-1/out")))
        .with(Role::Target, Path::new(&format!("{home}/ws/target")))
        .with(Role::Workspace, Path::new(&format!("{home}/ws")))
}

/// A checkout's files and environment, as a run there sees them.
struct Disk {
    files: BTreeMap<PathBuf, [u8; 32]>,
    env: Vec<(OsString, OsString)>,
}

impl Disk {
    fn of(home: &str, source: &[u8]) -> Disk {
        let mut files = BTreeMap::new();
        files.insert(
            PathBuf::from(format!("{home}/ws/p/build.rs")),
            ContentId::of("test file", b"fn main() {}").0,
        );
        files.insert(
            PathBuf::from(format!("{home}/ws/p/src/lib.c")),
            ContentId::of("test file", source).0,
        );
        let env = vec![
            ("CARGO_MANIFEST_DIR".into(), format!("{home}/ws/p").into()),
            ("OUT_DIR".into(), format!("{home}/ws/target/debug/build/p-1/out").into()),
            ("PATH".into(), "/usr/bin".into()),
        ];
        Disk { files, env }
    }
}

impl Inputs for Disk {
    type Error = String;
    fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, String> {
        Ok(self
            .files
            .get(path)
            .copied()
            .or_else(|| self.files.keys().any(|it| it.starts_with(path)).then_some(DIRECTORY)))
    }
    fn dir(&mut self, path: &Path) -> Result<Option<[u8; 32]>, String> {
        let names: Vec<&OsStr> =
            self.files.keys().filter_map(|it| it.strip_prefix(path).ok()?.iter().next()).collect();
        Ok((!names.is_empty()).then(|| listing(names)))
    }
    fn env(&mut self, name: &OsStr) -> Option<&OsStr> {
        self.env.iter().find(|it| it.0 == name).map(|it| it.1.as_os_str())
    }
    fn env_all(&mut self) -> &[(OsString, OsString)] {
        self.env.sort();
        &self.env
    }
    fn names(&mut self, _: &Path, _: &mut Names<'_>) -> Result<bool, String> {
        Ok(false)
    }
    fn entry(&mut self, _: &Path) -> Result<Entry, String> {
        Ok(Entry::Missing)
    }
}

/// The key a build script in `home`'s checkout is given, as `script::prepare` makes it.
fn key(home: &str, map: &RootMap) -> RunKey {
    let roots = map.view();
    let mut given = Given::new(RunKind::BuildScript);
    for (name, value) in [
        ("CARGO_MANIFEST_DIR", format!("{home}/ws/p")),
        ("OUT_DIR", format!("{home}/ws/target/debug/build/p-1/out")),
    ] {
        given.env(name, roots.os(OsStr::new(&value)).into_owned());
    }
    given.roots(&roots);
    given.ported(
        "program",
        format!("{home}/ws/target/debug/build/p-0/build-script-build").as_bytes(),
        &roots,
    );
    given.ported("dir", format!("{home}/ws/p").as_bytes(), &roots);
    RunKey { code: ContentId::of("test code", b"script"), given: given.finish() }
}

/// What the run in `home` read.
fn read(home: &str, disk: &Disk) -> Traced {
    let mut traced = Traced {
        files: disk.files.iter().map(|(path, hash)| (path.clone(), Some(*hash))).collect(),
        dirs: Vec::new(),
        env: vec![("OUT_DIR".into(), Some(format!("{home}/ws/target/debug/build/p-1/out").into()))],
        untraceable: Vec::new(),
    };
    trace::normalize(&mut traced);
    traced
}

/// A store of one kept run: its shape in slot 0 and its value under its key.
fn kept(run: &RunKey, traced: &Traced, value: &[u8]) -> BTreeMap<Key, Vec<u8>> {
    let mut store = BTreeMap::new();
    store.insert(index::slot(run, 0), Shape::of(traced).encode());
    store.insert(index::result_key(run, traced), value.to_vec());
    store
}

fn found(
    store: &BTreeMap<Key, Vec<u8>>,
    run: &RunKey,
    map: &RootMap,
    disk: &mut Disk,
) -> Option<Vec<u8>> {
    let found = index::search(
        |key| Ok(store.get(key).map(Vec::as_slice)),
        run,
        &mut Rooted::new(map.view(), disk),
    );
    match found.expect("the check reads the disk") {
        Found::Hit { value, .. } => Some(value.to_vec()),
        Found::Miss { .. } => None,
    }
}

#[test]
fn two_checkouts_at_different_paths_give_one_key() {
    let (a, b) = (checkout("/home/a"), checkout("/srv/b/c"));
    assert_eq!(key("/home/a", &a), key("/srv/b/c", &b));
    let (disk_a, mut disk_b) = (Disk::of("/home/a", b"int x;"), Disk::of("/srv/b/c", b"int x;"));
    let (ported_a, ported_b) =
        (a.view().traced(&read("/home/a", &disk_a)), b.view().traced(&read("/srv/b/c", &disk_b)));
    assert_eq!(ported_a, ported_b);
    let run = key("/home/a", &a);
    assert_eq!(run.traced(&ported_a), run.traced(&ported_b));
    // Kept in one checkout, found in the other through its own roots.
    let store = kept(&run, &ported_a, b"value");
    assert_eq!(found(&store, &key("/srv/b/c", &b), &b, &mut disk_b), Some(b"value".to_vec()));
}

#[test]
fn a_changed_file_under_a_root_changes_the_key() {
    let (a, b) = (checkout("/home/a"), checkout("/srv/b"));
    let disk_a = Disk::of("/home/a", b"int x;");
    let ported = a.view().traced(&read("/home/a", &disk_a));
    let run = key("/home/a", &a);
    let store = kept(&run, &ported, b"value");
    let mut changed = Disk::of("/srv/b", b"int y;");
    assert_eq!(found(&store, &key("/srv/b", &b), &b, &mut changed), None);
    let changed = b.view().traced(&read("/srv/b", &changed));
    assert_ne!(run.traced(&ported), run.traced(&changed));
}

#[test]
fn a_root_of_another_role_or_a_missing_one_gives_another_key() {
    let a = checkout("/home/a");
    let fewer = RootMap::default().with(Role::Package, Path::new("/home/a/ws/p"));
    assert_ne!(key("/home/a", &a), key("/home/a", &fewer));
    // A path under no root stays absolute: two checkouts' differ.
    let none = RootMap::default();
    assert_ne!(key("/home/a", &none), key("/home/b", &none));
}

#[test]
fn a_value_that_embeds_a_root_is_found_only_where_that_root_is() {
    let (a, b) = (checkout("/home/a"), checkout("/srv/b"));
    let disk_a = Disk::of("/home/a", b"int x;");
    let value = Value {
        code: Some(0),
        signal: None,
        stdout: b"cargo:rustc-link-search=/home/a/ws/target/debug/build/p-1/out\ncargo:warning=x/home/a/ws/p/x.rs\n".to_vec(),
        stderr: Vec::new(),
        files: Vec::new(),
    };
    let roots = a.view();
    let (stored, embedded) = roots.stored(&value).unwrap();
    assert!(embedded.contains(Role::Package) && !embedded.contains(Role::OutDir));
    assert_eq!(stored.stdout, b"cargo:rustc-link-search=\0o\ncargo:warning=x/home/a/ws/p/x.rs\n");
    let mut traced = roots.traced(&read("/home/a", &disk_a));
    roots.mark(&mut traced, embedded);
    let run = key("/home/a", &a);
    let store = kept(&run, &traced, b"value");
    // Found again where the package is.
    assert_eq!(
        found(&store, &run, &a, &mut Disk::of("/home/a", b"int x;")),
        Some(b"value".to_vec())
    );
    // Never in a checkout whose package is elsewhere.
    assert_eq!(found(&store, &key("/srv/b", &b), &b, &mut Disk::of("/srv/b", b"int x;")), None);
    // What it printed is this checkout's when it is reused.
    assert!(
        b.view()
            .restored(stored)
            .unwrap()
            .stdout
            .starts_with(b"cargo:rustc-link-search=/srv/b/ws/target/debug/build/p-1/out\n")
    );
}

#[test]
fn ported_strings_round_trip_and_never_collide() {
    let map = checkout("/home/a");
    let roots = map.view();
    for text in [
        &b"/home/a/ws/p"[..],
        b"-I/home/a/ws/p/include:/home/a/ws/target/debug/build/p-1/out",
        b"/home/a/ws/px",
        b"a\0b\0\0p",
        b"\0p",
        b"",
    ] {
        let ported = roots.ported(text);
        assert_eq!(roots.ported_len(text), ported.len());
        assert_eq!(&*roots.unport(&ported).unwrap(), text, "{:?}", String::from_utf8_lossy(text));
    }
    // A name that only starts with a root is not under it.
    assert_eq!(&*roots.ported(b"/home/a/ws/px"), b"\0w/px");
    // The nearer root wins, and a printed `\0p` is not a placeholder.
    assert_eq!(&*roots.ported(b"/home/a/ws/p/x"), b"\0p/x");
    assert_ne!(roots.ported(b"\0p"), roots.ported(b"/home/a/ws/p"));
    assert!(roots.unport(b"\0z").is_err());
    assert!(roots.unport(b"x\0").is_err());
}

#[test]
fn a_path_is_written_against_the_nearest_root_by_whole_names() {
    let map = checkout("/home/a");
    let roots = map.view();
    assert_eq!(&*roots.path(Path::new("/home/a/ws/p/build.rs")), Path::new("\0p/build.rs"));
    assert_eq!(
        &*roots.path(Path::new("/home/a/ws/target/debug/build/p-1/out/x")),
        Path::new("\0o/x")
    );
    assert_eq!(&*roots.path(Path::new("/home/a/ws/pq")), Path::new("\0w/pq"));
    assert_eq!(&*roots.path(Path::new("/usr/include/stdio.h")), Path::new("/usr/include/stdio.h"));
    let other = checkout("/srv/b");
    assert_eq!(
        &*other.view().real_path(Path::new("\0p/build.rs")).unwrap(),
        Path::new("/srv/b/ws/p/build.rs")
    );
    assert!(RootMap::default().view().real_path(Path::new("\0p/build.rs")).is_err());
    assert!(other.view().real_path(Path::new("\0pq")).is_err());
}

#[test]
fn a_root_that_is_not_absolute_or_is_the_file_systems_names_none() {
    let map = RootMap::default()
        .with(Role::Package, Path::new("ws/p"))
        .with(Role::Workspace, Path::new("/"));
    assert!(map.view().is_empty());
}

/// A run that read a file outside its roots and the machine-wide places (a sibling checkout's
/// `../../proto/x.proto`) is kept against where its roots are: another checkout, which would
/// check that same absolute path, never takes it.
#[test]
fn a_read_outside_the_roots_is_not_shared() {
    let (a, b) = (checkout("/home/a"), checkout("/srv/b"));
    let mut disk_a = Disk::of("/home/a", b"int x;");
    disk_a.files.insert(
        PathBuf::from("/home/proto/x.proto"),
        ContentId::of("test file", b"message X {}").0,
    );
    disk_a.files.insert(
        PathBuf::from("/usr/include/stdio.h"),
        ContentId::of("test file", b"int printf();").0,
    );
    let value = Value {
        code: Some(0),
        signal: None,
        stdout: Vec::new(),
        stderr: Vec::new(),
        files: Vec::new(),
    };
    let (kept, _) = a.view().kept(&read("/home/a", &disk_a), &value).unwrap();
    assert!(kept.shared.is_none(), "no copy for another checkout to verify");
    let run = key("/home/a", &a);
    let store = kept_store(&run, &kept.local);
    let mut disk_b = Disk::of("/srv/b", b"int x;");
    disk_b.files.extend(
        disk_a
            .files
            .iter()
            .filter(|it| !it.0.starts_with("/home/a"))
            .map(|(path, hash)| (path.clone(), *hash)),
    );
    assert_eq!(found(&store, &key("/srv/b", &b), &b, &mut disk_b), None);
    assert!(found(&store, &run, &a, &mut disk_a).is_some(), "found again where its roots are");
    // A read in a machine-wide place alone leaves a copy for another checkout to verify.
    let mut system = Disk::of("/home/a", b"int x;");
    system.files.insert(
        PathBuf::from("/usr/include/stdio.h"),
        ContentId::of("test file", b"int printf();").0,
    );
    let (kept, _) = a.view().kept(&read("/home/a", &system), &value).unwrap();
    assert!(kept.bound.is_empty() && kept.shared.is_some());
}

fn kept_store(run: &RunKey, traced: &Traced) -> BTreeMap<Key, Vec<u8>> {
    kept(run, traced, b"value")
}

/// Two checkouts whose roots sit otherwise to each other are two keys: a target directory
/// inside the workspace in one and elsewhere in the other, a package that is its workspace in
/// one and not the other.
#[test]
fn another_layout_of_the_roots_misses() {
    let inside = checkout("/home/a");
    let elsewhere = checkout("/srv/b").with(Role::Target, Path::new("/cache/target"));
    assert_ne!(key("/home/a", &inside), key("/srv/b", &elsewhere));
    let one = RootMap::default()
        .with(Role::Package, Path::new("/home/a/ws"))
        .with(Role::Workspace, Path::new("/home/a/ws"));
    let two = RootMap::default()
        .with(Role::Package, Path::new("/srv/b/ws/p"))
        .with(Role::Workspace, Path::new("/srv/b/ws"));
    let given = |map: &RootMap| {
        let mut given = Given::new(RunKind::BuildScript);
        given.roots(&map.view());
        given.finish()
    };
    assert_ne!(given(&one), given(&two));
    let same = RootMap::default()
        .with(Role::Package, Path::new("/srv/c/ws"))
        .with(Role::Workspace, Path::new("/srv/c/ws"));
    assert_eq!(given(&one), given(&same));
}

/// A root spelled otherwise (`/private` on macOS, separators doubled or turned, case,
/// `%`-escaped, UTF-16) is still found embedded, and only a root as it is is ported.
#[test]
fn a_root_in_another_spelling_is_embedded() {
    let map = RootMap::default().with(Role::Workspace, Path::new("/tmp/blaze roots/ws"));
    let roots = map.view();
    for spelled in [
        &b"x /private/tmp/blaze roots/ws/a.rs"[..],
        b"//tmp//blaze roots/ws/a.rs",
        b"C:\\tmp\\blaze roots\\ws",
        b"{\"at\":\"\\/tmp\\/blaze roots\\/ws\"}",
        b"/TMP/Blaze Roots/WS",
        b"file:///tmp/blaze%20roots/ws/a.rs",
    ] {
        assert!(
            roots.embedded(spelled).contains(Role::Workspace),
            "{:?}",
            String::from_utf8_lossy(spelled)
        );
        let value = Value {
            code: Some(0),
            signal: None,
            stdout: spelled.to_vec(),
            stderr: Vec::new(),
            files: Vec::new(),
        };
        assert!(
            roots.stored(&value).unwrap().1.contains(Role::Workspace),
            "{:?}",
            String::from_utf8_lossy(spelled)
        );
    }
    let wide: Vec<u8> = "/tmp/blaze roots/ws".encode_utf16().flat_map(u16::to_le_bytes).collect();
    assert!(roots.embedded(&wide).contains(Role::Workspace));
    assert_eq!(&*roots.ported(b"/private/tmp/blaze roots/ws"), b"/private/tmp/blaze roots/ws");
    assert!(roots.embedded(b"/tmp/blaze roots/other").is_empty());
}

/// A root inside a longer path is no placeholder: `/src` in `/usr/src/linux` stays, and what
/// it is in is kept against where the root is.
#[test]
fn a_root_inside_a_longer_path_is_not_ported() {
    let map = RootMap::default().with(Role::Package, Path::new("/src"));
    let roots = map.view();
    assert_eq!(&*roots.ported(b"/usr/src/linux"), b"/usr/src/linux");
    assert_eq!(&*roots.ported(b"-I/src/include:/src"), b"-I/src/include:\0p");
    let value = Value {
        code: Some(0),
        signal: None,
        stdout: b"/usr/src/linux\n".to_vec(),
        stderr: Vec::new(),
        files: Vec::new(),
    };
    assert!(roots.stored(&value).unwrap().1.contains(Role::Package));
}

/// A run that wrote files is kept against where its roots are, and leaves a copy: their bytes
/// and names, kept as they are and holding no root, are compared byte for byte by another
/// checkout before it is given them.
#[test]
fn a_run_that_wrote_files_is_kept_against_its_roots_until_verified() {
    let a = checkout("/home/a");
    let value = Value {
        code: Some(0),
        signal: None,
        stdout: Vec::new(),
        stderr: Vec::new(),
        files: vec![crate::run::OutFile {
            path: "5f3a-foo.o".into(),
            executable: false,
            bytes: Some(b"\x7fELF".to_vec()),
        }],
    };
    let (kept, _) =
        a.view().kept(&read("/home/a", &Disk::of("/home/a", b"int x;")), &value).unwrap();
    assert_eq!(marks(&kept.local), 4, "every root of the run is marked");
    assert_eq!(kept.shared.as_ref().map(marks), Some(0), "its copy has no mark");
}

fn marks(traced: &Traced) -> usize {
    traced.files.iter().filter(|it| it.0.ends_with(ROOT)).count()
}

fn printed(stdout: &[u8]) -> Value {
    Value {
        code: Some(0),
        signal: None,
        stdout: stdout.to_vec(),
        stderr: Vec::new(),
        files: Vec::new(),
    }
}

fn reading(paths: &[&str]) -> Traced {
    let mut traced = Traced {
        files: paths
            .iter()
            .map(|it| (PathBuf::from(it), Some(ContentId::of("test file", it.as_bytes()).0)))
            .collect(),
        dirs: Vec::new(),
        env: Vec::new(),
        untraceable: Vec::new(),
    };
    trace::normalize(&mut traced);
    traced
}

/// A root with a path byte before it is no placeholder (`/src` in `vendor/src/lib`): B would
/// write it back as `vendor/home/b/ws/lib`. It stays as it is and marks its root.
#[test]
fn a_root_after_a_name_is_kept_against_where_it_is() {
    let map = RootMap::default().with(Role::Workspace, Path::new("/src"));
    let roots = map.view();
    assert_eq!(&*roots.ported(b"vendor/src/lib"), b"vendor/src/lib");
    let (kept, stored) = roots.kept(&reading(&[]), &printed(b"vendor/src/lib\n")).unwrap();
    assert_eq!(stored.stdout, b"vendor/src/lib\n");
    assert!(kept.bound.contains(Role::Workspace) && kept.shared.is_none());
    assert_eq!(marks(&kept.local), 1);
}

/// A printed path from one root to another that the key says nothing of (a target directory
/// outside the workspace) marks both; one the layout fixes does not.
#[test]
fn a_path_between_roots_of_two_tops_is_kept_against_them() {
    let apart = RootMap::default()
        .with(Role::Package, Path::new("/home/a/ws/p"))
        .with(Role::Workspace, Path::new("/home/a/ws"))
        .with(Role::Target, Path::new("/cache/a/target"));
    let (_, embedded) = apart
        .view()
        .stored(&printed(b"cargo:rustc-env=T=../../../../cache/a/target/debug\n"))
        .unwrap();
    assert!(embedded.contains(Role::Package) && embedded.contains(Role::Target));
    let inside = checkout("/home/a");
    assert!(
        inside
            .view()
            .stored(&printed(b"cargo:rustc-env=T=../target/debug\n"))
            .unwrap()
            .1
            .is_empty()
    );
}

/// A git dependency's build script, its package in blaze's sources, reading `../` of it reads
/// another revision's files: marked. The registry, and `/nix/store` that blaze's tools are in,
/// stay machine-wide.
#[test]
fn a_read_next_to_a_root_in_blazes_sources_is_kept_against_it() {
    let map = RootMap::default()
        .with(Role::Package, Path::new("/b/sources/git/checkouts/dep-1/abc"))
        .with(Role::OutDir, Path::new("/h/ws/target/debug/build/dep-1/out"))
        .with(Role::Target, Path::new("/h/ws/target"))
        .with(Role::Workspace, Path::new("/h/ws"))
        .with(Role::Tools, Path::new("/nix/store/x-blaze/bin"))
        .with_sources(Path::new("/b/sources"));
    let roots = map.view();
    let (kept, _) = roots
        .kept(&reading(&["/b/sources/git/checkouts/dep-1/proto/x.proto"]), &printed(b""))
        .unwrap();
    assert!(kept.shared.is_none());
    assert_eq!(kept.bound, roots.all());
    let (kept, _) = roots
        .kept(
            &reading(&[
                "/b/sources/registry/src/index/foo-1.0/lib.rs",
                "/nix/store/y-zlib/include/zlib.h",
            ]),
            &printed(b""),
        )
        .unwrap();
    assert!(kept.bound.is_empty() && kept.shared.is_some());
}

/// What `puts` say, kept in `store` as [`index::keep`] keeps them: each value under its key,
/// never over another, and its shape in the first free slot of its run's chain.
fn apply(store: &mut BTreeMap<Key, Vec<u8>>, puts: Vec<verify::Put>) {
    for put in puts {
        let key = index::result_key(&put.run, &put.traced);
        match store.get(&key) {
            Some(old) => assert_eq!(old, &put.value, "two values under one key"),
            None => {
                store.insert(key, put.value);
            }
        }
        let shape = Shape::of(&put.traced).encode();
        for n in 0.. {
            let at = index::slot(&put.run, n);
            match store.get(&at) {
                Some(it) if *it == shape => break,
                Some(_) => continue,
                None => {
                    store.insert(at, shape);
                    break;
                }
            }
        }
    }
}

/// The run of `home`'s checkout that prints `stdout`, kept in `store` with only what the store
/// holds: no state of a verify lives anywhere else, so a daemon restored from the store says
/// what one that never stopped says.
fn ran(
    store: &mut BTreeMap<Key, Vec<u8>>,
    home: &str,
    map: &RootMap,
    stdout: &[u8],
) -> verify::Verdict {
    ran_value(store, home, map, &printed(stdout))
}

/// [`ran`] of a run that gave `value`.
fn ran_value(
    store: &mut BTreeMap<Key, Vec<u8>>,
    home: &str,
    map: &RootMap,
    value: &Value,
) -> verify::Verdict {
    let (puts, verdict) = keeps_in(store, home, map, value);
    apply(store, puts);
    verdict
}

/// What [`verify::keeps`] says to put of the run of `home`'s checkout that gave `value`, the
/// store as it is: nothing applied, so two checkouts can keep at once.
fn keeps_in(
    store: &BTreeMap<Key, Vec<u8>>,
    home: &str,
    map: &RootMap,
    value: &Value,
) -> (Vec<verify::Put>, verify::Verdict) {
    let disk = Disk::of(home, b"int x;");
    let roots = map.view();
    let (marked, stored) = roots.kept(&read(home, &disk), value).unwrap();
    verify::keeps(
        |key| Ok(store.get(key).map(Vec::as_slice)),
        &roots,
        &key(home, map),
        marked,
        stored.encode(),
        crate::run::ported_differs,
    )
    .unwrap()
}

fn found_in(store: &BTreeMap<Key, Vec<u8>>, home: &str, map: &RootMap) -> Option<Vec<u8>> {
    found(store, &key(home, map), map, &mut Disk::of(home, b"int x;"))
}

/// The store of `sim` in `store`, opened (again, after a drop: what a restarted daemon reads).
fn opened(sim: &mut blaze_core::sim::Sim) -> blaze_cas::Store {
    let mut effects = Vec::new();
    let config = blaze_cas::Config {
        dir: blaze_core::FileName::new("store").unwrap(),
        layers: Vec::new(),
        tags: 1 << 48,
    };
    let mut store = blaze_cas::Store::open(config, &mut effects);
    blaze_core::run_until_idle(sim, &mut effects, |completion, effects| {
        _ = store.complete(completion, effects);
    }).expect("the effects end");
    assert!(store.ready());
    store
}

/// Everything put in `store`, durable.
fn flushed(sim: &mut blaze_core::sim::Sim, store: &mut blaze_cas::Store) {
    let mut effects = Vec::new();
    store.flush(&mut effects);
    blaze_core::run_until_idle(sim, &mut effects, |completion, effects| {
        _ = store.complete(completion, effects);
    }).expect("the effects end");
}

/// [`ran`] into a real store, through [`verify::keep`].
fn ran_kept(
    store: &mut blaze_cas::Store,
    home: &str,
    map: &RootMap,
    stdout: &[u8],
) -> verify::Verdict {
    let disk = Disk::of(home, b"int x;");
    let roots = map.view();
    let (marked, stored) = roots.kept(&read(home, &disk), &printed(stdout)).unwrap();
    let kept = verify::keep(
        store,
        &roots,
        &key(home, map),
        marked,
        stored.encode(),
        crate::run::ported_differs,
    )
    .unwrap();
    kept.verdict
}

/// [`found_in`] of a real store.
fn found_kept(store: &blaze_cas::Store, home: &str, map: &RootMap) -> Option<Vec<u8>> {
    let mut disk = Disk::of(home, b"int x;");
    let found = index::search(
        |key| Ok(store.get(key)?.map(|it| it.bytes)),
        &key(home, map),
        &mut Rooted::new(map.view(), &mut disk),
    );
    match found.expect("the check reads the disk") {
        Found::Hit { value, .. } => Some(value.to_vec()),
        Found::Miss { .. } => None,
    }
}

/// A run is never given to another checkout on the word of the one that ran it: the first
/// keeps it against its roots and a copy; a second that runs it and gets the same, its
/// output ported, verifies it, and every other checkout is given it from then on. All of it
/// is the store's: the store flushed, dropped and opened again (a daemon restarted) says at
/// each step what the one that never stopped said (rule 26).
#[test]
fn another_checkout_verifies_a_run_before_it_is_given_it() {
    let mut sim = blaze_core::sim::Sim::new(11, blaze_core::sim::Faults::default());
    let (a, b, c) = (checkout("/home/a"), checkout("/srv/b"), checkout("/opt/c"));
    let printed = b"cargo:rustc-link-search=/home/a/ws/target/debug/build/p-1/out\n";
    let mut store = opened(&mut sim);
    assert_eq!(ran_kept(&mut store, "/home/a", &a, printed), verify::Verdict::Copied);
    let here = found_kept(&store, "/home/a", &a).expect("found again where it ran");
    assert_eq!(found_kept(&store, "/srv/b", &b), None, "never given another checkout unverified");
    flushed(&mut sim, &mut store);
    drop(store);
    let mut store = opened(&mut sim);
    assert_eq!(
        found_kept(&store, "/home/a", &a),
        Some(here.clone()),
        "restored, found where it ran"
    );
    assert_eq!(
        found_kept(&store, "/srv/b", &b),
        None,
        "restored, still not given another checkout"
    );

    let b_printed = b"cargo:rustc-link-search=/srv/b/ws/target/debug/build/p-1/out\n";
    assert_eq!(ran_kept(&mut store, "/srv/b", &b, b_printed), verify::Verdict::Verified);
    let fresh = found_kept(&store, "/opt/c", &c).expect("verified, given to a third checkout");
    flushed(&mut sim, &mut store);
    drop(store);
    let store = opened(&mut sim);
    let given =
        found_kept(&store, "/opt/c", &c).expect("restored, verified, given to a third checkout");
    assert_eq!(given, fresh, "the restored store gives the bytes the running one did");
    let given = c.view().restored(Value::decode(&given).unwrap()).unwrap();
    assert_eq!(given.stdout, b"cargo:rustc-link-search=/opt/c/ws/target/debug/build/p-1/out\n");
    assert_eq!(found_kept(&store, "/home/a", &a), Some(here), "and still where it first ran");
}

/// The roots of two checkouts that differ in their target directory alone: the package,
/// workspace and tools the same, the target directory outside the workspace in both.
fn sharing(target: &str, tools: &str) -> RootMap {
    RootMap::default()
        .with(Role::Package, Path::new("/h/ws/p"))
        .with(Role::Workspace, Path::new("/h/ws"))
        .with(Role::Target, Path::new(target))
        .with(Role::Tools, Path::new(tools))
}

/// C1 (round 4): two checkouts that agree say nothing of a root that is the same in both. A
/// value verified by checkouts that differ only in their target directory is given to a third
/// whose target directory is elsewhere, but not to one whose tools are: the tools stay marked.
#[test]
fn verifying_across_one_root_keeps_the_others_marked() {
    let (a, b) = (sharing("/ta/target", "/opt/tools"), sharing("/tb/target", "/opt/tools"));
    assert_eq!(key("/h", &a), key("/h", &b), "one key: only the target directory moved");
    let mut store = BTreeMap::new();
    assert_eq!(ran(&mut store, "/h", &a, b"cargo:rustc-cfg=x\n"), verify::Verdict::Copied);
    assert_eq!(ran(&mut store, "/h", &b, b"cargo:rustc-cfg=x\n"), verify::Verdict::Verified);
    let target_elsewhere = sharing("/tc/target", "/opt/tools");
    assert!(
        found_in(&store, "/h", &target_elsewhere).is_some(),
        "a root seen to differ is not marked"
    );
    let tools_elsewhere = sharing("/tc/target", "/opt/other-tools");
    assert_eq!(key("/h", &tools_elsewhere), key("/h", &a));
    assert_eq!(
        found_in(&store, "/h", &tools_elsewhere),
        None,
        "a root the same in both checkouts stays marked"
    );
}

/// N4 (round 5): a pair of checkouts that agree is evidence for the roots that differed in that
/// pair alone. Two checkouts keep at once, so a third finds both copies: one differs from it in
/// the target directory, the other in the tools. Each pair's roots go unmarked in a verified
/// put of their own, never together: a checkout with both elsewhere was shown by no pair.
#[test]
fn a_pair_verifies_only_the_roots_that_differed_in_it() {
    let tools = "/opt/k-tools";
    let k = sharing("/tk/target", tools);
    let (a, b) = (sharing("/ta/target", tools), sharing("/tk/target", "/opt/b-tools"));
    let x = printed(b"cargo:rustc-cfg=x\n");
    let mut store = BTreeMap::new();
    // At once: neither sees the other's copy.
    let (puts_a, verdict_a) = keeps_in(&store, "/h", &a, &x);
    let (puts_b, verdict_b) = keeps_in(&store, "/h", &b, &x);
    assert_eq!((verdict_a, verdict_b), (verify::Verdict::Copied, verify::Verdict::Copied));
    apply(&mut store, puts_a);
    apply(&mut store, puts_b);
    let (puts, verdict) = keeps_in(&store, "/h", &k, &x);
    assert_eq!(verdict, verify::Verdict::Verified);
    assert_eq!(puts.len(), 2, "one verified put per pair's set of roots: {puts:?}");
    apply(&mut store, puts);
    assert!(
        found_in(&store, "/h", &sharing("/td/target", tools)).is_some(),
        "target elsewhere: shown by k and a"
    );
    assert!(
        found_in(&store, "/h", &sharing("/tk/target", "/opt/d-tools")).is_some(),
        "tools elsewhere: shown by k and b"
    );
    assert_eq!(
        found_in(&store, "/h", &sharing("/td/target", "/opt/d-tools")),
        None,
        "both elsewhere: shown by no pair"
    );
}

/// D1 (round 5): the record that a run depends on where its roots are, put when another
/// checkout's copy differs, is the one put marked durable, and its ticket is what the run is
/// held for until the store says it is durable: a crash before then can't forget it.
#[test]
fn a_record_that_a_run_depends_on_its_roots_is_held_durable() {
    let (a, b) = (checkout("/home/a"), checkout("/srv/b"));
    let mut store = BTreeMap::new();
    assert_eq!(ran(&mut store, "/home/a", &a, b"cargo:rustc-cfg=a\n"), verify::Verdict::Copied);
    let (puts, verdict) = keeps_in(&store, "/srv/b", &b, &printed(b"cargo:rustc-cfg=b\n"));
    assert!(matches!(verdict, verify::Verdict::Depends { .. }), "{verdict:?}");
    let durable: Vec<&verify::Put> = puts.iter().filter(|it| it.durable).collect();
    assert_eq!(durable.len(), 1, "{puts:?}");
    assert_eq!(durable[0].value, verify::DEPENDS);
    assert!(
        puts.iter().filter(|it| !it.durable).all(|it| it.run == key("/srv/b", &b)),
        "its own value is no record"
    );

    let mut sim = blaze_core::sim::Sim::new(12, blaze_core::sim::Faults::default());
    let mut real = opened(&mut sim);
    assert_eq!(ran_kept(&mut real, "/home/a", &a, b"cargo:rustc-cfg=a\n"), verify::Verdict::Copied);
    let disk = Disk::of("/srv/b", b"int x;");
    let roots = b.view();
    let (marked, stored) =
        roots.kept(&read("/srv/b", &disk), &printed(b"cargo:rustc-cfg=b\n")).unwrap();
    let kept = verify::keep(
        &mut real,
        &roots,
        &key("/srv/b", &b),
        marked,
        stored.encode(),
        crate::run::ported_differs,
    )
    .unwrap();
    let ticket = kept.retracted.expect("the record's ticket is handed back");
    assert!(!real.durable(ticket), "not durable before a flush, so the run is held");
    flushed(&mut sim, &mut real);
    assert!(real.durable(ticket));
    drop(real);
    let real = opened(&mut sim);
    assert_eq!(
        found_kept(&real, "/var/d", &checkout("/var/d")),
        None,
        "restored, the record still keeps it from others"
    );
}

/// C1 (round 4): a file a run wrote that holds a root is tied to where that root is, even
/// when every checkout that ran it has the root in the same place (the tools, a target
/// directory they share): comparing them can't see it, so it is kept here alone, with no copy.
#[test]
fn a_written_file_that_embeds_a_root_is_kept_here_alone() {
    let (a, b) = (sharing("/ta/target", "/opt/tools"), sharing("/tb/target", "/opt/tools"));
    let wrote = Value {
        code: Some(0),
        signal: None,
        stdout: Vec::new(),
        stderr: Vec::new(),
        files: vec![crate::run::OutFile {
            path: "flags".into(),
            executable: false,
            bytes: Some(b"-B/opt/tools/bin".to_vec()),
        }],
    };
    let (kept, _) = a.view().kept(&read("/h", &Disk::of("/h", b"int x;")), &wrote).unwrap();
    assert!(kept.bound.contains(Role::Tools) && kept.shared.is_none(), "{kept:?}");
    let mut store = BTreeMap::new();
    assert_eq!(ran_value(&mut store, "/h", &a, &wrote), verify::Verdict::Here);
    assert_eq!(ran_value(&mut store, "/h", &b, &wrote), verify::Verdict::Here);
    assert!(found_in(&store, "/h", &a).is_some(), "found where it ran");
    assert_eq!(found_in(&store, "/h", &sharing("/tc/target", "/opt/other-tools")), None);
    assert_eq!(found_in(&store, "/h", &sharing("/tc/target", "/opt/tools")), None, "never shared");
}

/// P1 (round 5): the one-pass scan of written files finds a root in every spelling
/// [`Roots::embedded`] does: separators doubled and turned, another case, UTF-16, and a path
/// between roots of two tops; and nothing in a file that holds none.
#[test]
fn the_written_file_scan_finds_every_spelling_embedded_does() {
    let map = sharing("/ta/target", "/opt/tools");
    let roots = map.view();
    let wrote = |bytes: Vec<u8>| Value {
        code: Some(0),
        signal: None,
        stdout: Vec::new(),
        stderr: Vec::new(),
        files: vec![crate::run::OutFile {
            path: "f".into(),
            executable: false,
            bytes: Some(bytes),
        }],
    };
    let wide: Vec<u8> = "/opt/tools".encode_utf16().flat_map(u16::to_le_bytes).collect();
    let relative = super::relative("/h/ws/p", "/ta/target");
    for bytes in [
        b"-B//OPT\\Tools/bin".to_vec(),
        wide,
        relative.clone().into_bytes(),
        b"x /h/ws/p/src".to_vec(),
    ] {
        assert_eq!(
            roots.stored(&wrote(bytes.clone())).unwrap().1.is_empty(),
            roots.embedded(&bytes).is_empty(),
            "{bytes:?}"
        );
        assert!(!roots.stored(&wrote(bytes.clone())).unwrap().1.is_empty(), "{bytes:?}");
    }
    assert!(roots.stored(&wrote(b"int x; /usr/include".to_vec())).unwrap().1.is_empty());
}

/// A second checkout that gets otherwise records that the run depends on where its roots are:
/// each checkout keeps its own from then on, a third that agrees with the first verifies
/// nothing, and a value verified before is never given again.
#[test]
fn a_run_another_checkout_gets_otherwise_is_kept_where_it_ran() {
    let (a, b, c) = (checkout("/home/a"), checkout("/srv/b"), checkout("/opt/c"));
    let mut store = BTreeMap::new();
    assert_eq!(ran(&mut store, "/home/a", &a, b"cargo:rustc-cfg=a\n"), verify::Verdict::Copied);
    assert!(matches!(
        ran(&mut store, "/srv/b", &b, b"cargo:rustc-cfg=b\n"),
        verify::Verdict::Depends { .. }
    ));
    assert!(found_in(&store, "/srv/b", &b).is_some(), "kept where it ran");
    assert!(matches!(
        ran(&mut store, "/opt/c", &c, b"cargo:rustc-cfg=a\n"),
        verify::Verdict::Depends { .. }
    ));
    assert!(found_in(&store, "/opt/c", &c).is_some());
    let d = checkout("/var/d");
    assert_eq!(found_in(&store, "/var/d", &d), None, "nothing verified");

    // Verified by two that agree, then shown wrong by a third: never given again.
    let mut store = BTreeMap::new();
    assert_eq!(ran(&mut store, "/home/a", &a, b"cargo:rustc-cfg=a\n"), verify::Verdict::Copied);
    assert_eq!(ran(&mut store, "/srv/b", &b, b"cargo:rustc-cfg=a\n"), verify::Verdict::Verified);
    assert!(found_in(&store, "/var/d", &d).is_some());
    assert!(matches!(
        ran(&mut store, "/opt/c", &c, b"cargo:rustc-cfg=c\n"),
        verify::Verdict::Depends { .. }
    ));
    assert_eq!(found_in(&store, "/var/d", &d), None, "the verified value is not given again");
    assert!(found_in(&store, "/opt/c", &c).is_some(), "each keeps its own");
}

/// A variable a run read that holds a root after a path byte (`-I<root>/include`, the same text
/// in both checkouts) is no placeholder: B's run would read A's files through it, so A's run
/// leaves no copy and B misses.
#[test]
fn a_root_left_in_a_variable_read_is_kept_against_where_it_is() {
    let (a, b) = (checkout("/home/a"), checkout("/srv/b"));
    let header = ContentId::of("test file", b"int foo;").0;
    let mut disk_a = Disk::of("/home/a", b"int x;");
    disk_a.files.insert(PathBuf::from("/home/a/ws/p/include/foo.h"), header);
    let mut traced = read("/home/a", &disk_a);
    traced.env.push(("CFLAGS".into(), Some("-I/home/a/ws/p/include".into())));
    trace::normalize(&mut traced);
    let roots = a.view();
    let (marked, stored) = roots.kept(&traced, &printed(b"")).unwrap();
    assert!(marked.bound.contains(Role::Package) && marked.shared.is_none());
    let (puts, verdict) = verify::keeps(
        |_| Ok(None),
        &roots,
        &key("/home/a", &a),
        marked,
        stored.encode(),
        crate::run::ported_differs,
    )
    .unwrap();
    assert_eq!(verdict, verify::Verdict::Here);
    let mut store = BTreeMap::new();
    apply(&mut store, puts);
    let mut disk_b = Disk::of("/srv/b", b"int x;");
    disk_b.files.insert(PathBuf::from("/srv/b/ws/p/include/foo.h"), header);
    disk_b.env.push(("CFLAGS".into(), "-I/home/a/ws/p/include".into()));
    assert_eq!(found(&store, &key("/srv/b", &b), &b, &mut disk_b), None);
}

/// A string the key was given that holds a root once ported ties the run to its checkout too.
#[test]
fn a_root_left_in_a_given_string_is_kept_against_where_it_is() {
    let mut a = checkout("/home/a");
    let left = a.view().left(b"-L/home/a/ws/p/lib");
    assert!(left.contains(Role::Package));
    assert!(a.view().left(b"/home/a/ws/p/lib").is_empty(), "a root at a path's start is ported");
    a.bind(left);
    let (kept, _) =
        a.view().kept(&read("/home/a", &Disk::of("/home/a", b"int x;")), &printed(b"")).unwrap();
    assert!(kept.shared.is_none());
}

/// A package that is its workspace (one crate's repo) is no path between roots: clean output
/// leaves nothing bound, the copy has no marks, and the layout is the same in every checkout.
#[test]
fn a_package_that_is_its_workspace_marks_nothing() {
    let at = |home: &str| {
        RootMap::default()
            .with(Role::Package, Path::new(&format!("{home}/p")))
            .with(Role::Workspace, Path::new(&format!("{home}/p")))
            .with(Role::Target, Path::new(&format!("{home}/p/target")))
            .with(Role::OutDir, Path::new(&format!("{home}/p/target/debug/build/p-1/out")))
    };
    let a = at("/home/a");
    let roots = a.view();
    assert_eq!(roots.top(Role::Package), roots.top(Role::Workspace));
    assert!(roots.embedded(b"cargo:rustc-cfg=x\n").is_empty());
    let (kept, stored) =
        roots.kept(&reading(&["/home/a/p/build.rs"]), &printed(b"cargo:rustc-cfg=x\n")).unwrap();
    assert!(roots.stored(&printed(b"cargo:rustc-cfg=x\n")).unwrap().1.is_empty());
    assert_eq!(stored.stdout, b"cargo:rustc-cfg=x\n");
    assert!(kept.bound.is_empty());
    assert_eq!(kept.shared.as_ref().map(marks), Some(0));
    let layout = |map: &RootMap| {
        let mut given = Given::new(RunKind::BuildScript);
        given.roots(&map.view());
        given.finish()
    };
    assert_eq!(layout(&a), layout(&at("/srv/b")));
}

/// A root whose link is re-pointed after its map was made ties the run to its checkout: the
/// spellings its output was scanned for may not be the ones it saw.
#[test]
fn a_root_relinked_since_its_map_was_made_is_kept_against_where_it_is() {
    let dir = std::env::temp_dir().join(format!("blaze-roots-relinked-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("one")).unwrap();
    std::fs::create_dir_all(dir.join("two")).unwrap();
    std::os::unix::fs::symlink(dir.join("one"), dir.join("ws")).unwrap();
    let map = RootMap::default().with(Role::Workspace, &dir.join("ws"));
    let (kept, _) = map.view().kept(&reading(&[]), &printed(b"")).unwrap();
    assert!(!kept.relinked && kept.shared.is_some());
    std::fs::remove_file(dir.join("ws")).unwrap();
    std::os::unix::fs::symlink(dir.join("two"), dir.join("ws")).unwrap();
    assert!(map.view().relinked());
    let (kept, _) = map.view().kept(&reading(&[]), &printed(b"")).unwrap();
    assert!(kept.relinked && kept.shared.is_none());
    _ = std::fs::remove_dir_all(&dir);
}
