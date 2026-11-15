//! The census of std's surfaces (rule 82): every library crate of the pinned standard library
//! under `<repo>/library`, as the no-daemon command makes it, and nothing of a crate written
//! here by hand:
//!
//! - std's graph as the load resolves it (`blaze_units::stdlib::cfg::std_graph`,
//!   `-Zbuild-std`'s defaults for a dev build on the host, `stdlib::cfg::dev_key`): which crates,
//!   their editions, roots and dependencies by the names their code uses;
//! - each unit's cfg and variables from the one source the command's graph takes them from
//!   (`stdlib::cfg::std_cfg`: the target's cfg, the profile, the features the load resolved,
//!   the ported script's output);
//! - its files by the one gather (`blaze_unitkey::gather::gather`, through a disk [`Source`]):
//!   the root, its modules (a macro call's candidates too), the files its late reads include.
//!
//! Each crate is put in one sealed database, in a library source root of its own at its real
//! path (as `blaze_command::snapshot::open` puts a surface task's crate), and run through
//! [`image_parts`], the surface task's path, in dependency order with the [`census`] taken; each
//! crate whose surface is made is then given by its address in a second sealed database, as a
//! body task's snapshot gives it, and its own def map read back ([`read_back`]). The one failure
//! lists every refusal of every crate by crate, kind and place (each refusal's text names its
//! file and line), where one release run of the command finds one.
//!
//! A census, never a way to build: the switch exists in this crate's test build alone
//! (`serve::census`), and what a crate does after a recorded refusal is never a record.
//!
//! The library is found as blaze-unitkey's `std_tests` finds it: the directory
//! `BLAZE_LIBRARY` names, else `<repo>/library`. A library that is not there fails the test,
//! naming the path it looked at: it is never skipped.

use std::{
    alloc::Global,
    fmt::Write as _,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
};

use base_db::{
    AbsPathBuf, ConstEvalBudget, CrateBuilderId, CrateDisplayName, CrateGraphBuilder, CrateKey,
    CrateName, CrateOrigin, CrateWorkspaceData, Db, DependencyBuilder, Env, FileId, FileSet,
    LangCrateOrigin, SourceRoot, SourceRootId, TargetKind, Version, VfsPath,
};
use blaze_format::Aligned;
use blaze_hash::HashSet;
use blaze_ids::ContentId;
use blaze_keyed::kind::{Namespace, ToolId};
use blaze_unitkey::{
    gather::{self, Gather, ImageList, Source, SourceError},
    image::Reading,
};
use blaze_units::stdlib;
use cfg::CfgOptions;
use intern::Symbol;
use syntax::{Edition, Extensions};

use super::{image_parts, serve::census};

/// What blaze's locator reads first (blaze-cargo `toolchain::library::VAR`).
const LIBRARY_VAR: &str = "BLAZE_LIBRARY";

const TOOL: ToolId = ToolId { tool: ContentId([7; 32]), target: ContentId([8; 32]), namespace: Namespace::PUBLIC };

/// One library crate of std's graph, as the command's graph gives it: its name, its edition,
/// what it is to the front end, its cfg and variables, its package's directory, and its
/// dependencies by the name its code calls each and the index in the census's crates of each.
struct StdCrate {
    name: String,
    edition: Edition,
    lang: LangCrateOrigin,
    cfg: CfgOptions,
    env: Vec<(String, String), Global>,
    dir: PathBuf,
    deps: Vec<Dep, Global>,
    /// Its gathered files, the root first, then what its late reads include; each `(path,
    /// bytes)`.
    files: Vec<(PathBuf, Vec<u8>), Global>,
}

/// One dependency edge of a [`StdCrate`].
struct Dep {
    name: String,
    /// The unit depended on, by its index in std's graph (resolved to a census crate when the
    /// graph is built).
    unit: usize,
    /// Its marks, by the one rule (`blaze_units::schema::UnitDep::marks`).
    marks: blaze_units::schema::EdgeMarks,
}

/// The library's directory, absolute and with no `..`, or the test fails naming where it
/// looked.
fn library() -> PathBuf {
    let dir = match std::env::var_os(LIBRARY_VAR).filter(|it| !it.is_empty()) {
        Some(named) => PathBuf::from(named),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../library"),
    };
    let marker = dir.join("core/src/lib.rs");
    assert!(
        marker.is_file(),
        "the standard library's sources are not at {}: it has no {} ({LIBRARY_VAR} names another `library/` directory)",
        dir.display(),
        marker.display()
    );
    std::fs::canonicalize(&dir).unwrap_or_else(|error| panic!("the standard library at {}: {error}", dir.display()))
}

/// Every file under `dir`, by its path under `library` (`/` separated), with its bytes: what
/// std's graph is read from (as `blaze_unitkey`'s std test hands it).
fn files_under(library: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>), Global>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    for entry in entries {
        let path = entry.map_err(|error| format!("{}: {error}", dir.display()))?.path();
        let meta = std::fs::metadata(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        if meta.is_dir() {
            files_under(library, &path, out)?;
            continue;
        }
        let relative =
            path.strip_prefix(library).map_err(|_| format!("{} is not under {}", path.display(), library.display()))?;
        let named = relative.to_str().ok_or_else(|| format!("{}: not UTF-8", path.display()))?;
        let bytes = std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        out.push((named.to_owned(), bytes));
    }
    Ok(())
}

/// The host's target tuple, as the command names it (`blaze_command::front::host`).
fn host() -> &'static str {
    match (cfg!(target_arch = "aarch64"), cfg!(target_os = "macos"), cfg!(target_os = "linux")) {
        (true, true, _) => "aarch64-apple-darwin",
        (true, _, true) => "aarch64-unknown-linux-gnu",
        (false, _, true) => "x86_64-unknown-linux-gnu",
        _ => panic!("the test runs on a host blaze has a target for (aarch64 macOS, aarch64 or x86-64 Linux)"),
    }
}

/// The cfg lines (`flag`, `key="value"`) as options, as the command's `front::cfg_of` reads
/// them.
fn options_of(lines: &[String]) -> CfgOptions {
    let mut options = CfgOptions::default();
    for line in lines {
        match line.split_once('=') {
            None => options.insert_atom(Symbol::intern(line.trim())),
            Some((key, value)) => {
                options.insert_key_value(Symbol::intern(key.trim()), Symbol::intern(value.trim().trim_matches('"')))
            }
        }
    }
    options
}

/// What every crate shares, as `blaze_command::front::workspace_of` makes it for the host.
fn workspace() -> triomphe::Arc<CrateWorkspaceData> {
    use blaze_units::stdlib::target;
    let triple = host();
    let data_layout = target::data_layout(triple).unwrap_or_else(|why| panic!("the data layout of {triple}: {why}"));
    let has_thumb_interworking =
        target::has_thumb_interworking(triple).unwrap_or_else(|why| panic!("thumb interworking of {triple}: {why}"));
    let version = target::rustc_version();
    let number = version.split(' ').next().unwrap_or(version);
    let toolchain = Version::parse(number).unwrap_or_else(|why| panic!("blaze's toolchain version `{version}`: {why}"));
    triomphe::Arc::new(CrateWorkspaceData {
        target: Ok(base_db::target::TargetData {
            tuple: Symbol::intern(triple),
            data_layout: data_layout.into_boxed_str(),
            arch: base_db::target::Arch::Other,
            has_thumb_interworking,
        }),
        toolchain: Some(toolchain),
        const_eval: ConstEvalBudget::default(),
        trusted: Box::default(),
    })
}

/// One line of the census for crate `name` into `out`.
fn note(out: &mut String, name: &str, why: &str) {
    let _ = writeln!(out, "`{name}`: {why}");
}

/// The disk, for the library's files: the census's [`Source`].
struct Disk;

impl Source for Disk {
    fn read<B: core::alloc::Allocator>(&mut self, path: &str, into: &mut Vec<u8, B>) -> Result<bool, SourceError> {
        into.clear();
        match std::fs::read(path) {
            Ok(bytes) => {
                into.extend_from_slice(&bytes);
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err("the std census: a library file does not read"),
        }
    }

    fn exists(&mut self, path: &str) -> Result<bool, SourceError> {
        match std::fs::metadata(path) {
            Ok(meta) => Ok(meta.is_file()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err("the std census: a library path does not stat"),
        }
    }
}

/// The census's own digests of late answers: which variable or file is read, and whether it
/// is there, are the gather's; only the hash is the census's.
struct CensusDigests;

impl gather::Digests for CensusDigests {
    fn var(value: Option<&[u8]>) -> ContentId {
        match value {
            Some(value) => ContentId::of("hir-def std census variable", value),
            None => ContentId::of("hir-def std census absent variable", b""),
        }
    }

    fn file(bytes: &[u8]) -> ContentId {
        ContentId::of("hir-def std census file", bytes)
    }
}

/// A variable row as the gather reads it.
fn pair(row: &(String, String)) -> (&[u8], &[u8]) {
    (row.0.as_bytes(), row.1.as_bytes())
}

/// What a crate of `name` is to the front end.
fn lang_of(name: &str) -> LangCrateOrigin {
    match name {
        "core" => LangCrateOrigin::Core,
        "alloc" => LangCrateOrigin::Alloc,
        "std" => LangCrateOrigin::Std,
        "proc_macro" => LangCrateOrigin::ProcMacro,
        "test" => LangCrateOrigin::Test,
        _ => LangCrateOrigin::Dependency,
    }
}

/// Every library crate of std's graph, gathered: each unit's cfg and variables by
/// `stdlib::cfg::std_cfg`, its files by `gather::gather`, in dependency order (each after every
/// crate it depends on). What the command refuses a unit for is noted into `out` and the unit
/// left out.
fn std_crates(library: &Path, out: &mut String) -> Vec<StdCrate, Global> {
    let mut files: Vec<(String, Vec<u8>), Global> = Vec::new_in(Global);
    files_under(library, library, &mut files).unwrap_or_else(|why| panic!("reading the library: {why}"));
    let named: Vec<(&str, &[u8]), Global> = {
        let mut named = Vec::with_capacity_in(files.len(), Global);
        named.extend(files.iter().map(|(path, bytes)| (path.as_str(), &bytes[..])));
        named
    };
    let graph = stdlib::cfg::std_graph(&named, |mount| stdlib::cfg::dev_key(mount, host()))
        .unwrap_or_else(|why| panic!("std's graph from {}: {why}", library.display()));
    let upstream = named
        .iter()
        .find(|(path, _)| *path == "UPSTREAM")
        .map(|(_, bytes)| String::from_utf8_lossy(bytes))
        .expect("the graph read UPSTREAM");
    let commit = stdlib::library_commit(&upstream).unwrap_or_else(|why| panic!("{}/UPSTREAM: {why}", library.display()));
    let named_at = stdlib::library_at(&commit);
    // blaze's stand-ins for `rustc` and `rustdoc` are named in a script's environment, never
    // run: the scripts are ported.
    let tools = std::env::temp_dir().join("hir-def-std-census-tools");
    let run = stdlib::cfg::ScriptRun { library: &named_at, dir: library, host: host(), tools: &tools, inherited: &[], jobs: 1 };
    let mut images = ImageList::new_in(Global);

    // Each library unit by its index in the graph, gathered.
    let mut gathered: Vec<(usize, StdCrate), Global> = Vec::new_in(Global);
    for (at, unit) in graph.units.iter().enumerate() {
        if !unit.target.is_lib() {
            continue;
        }
        let name = unit.target.name.replace('-', "_");
        let std = match stdlib::cfg::std_cfg(&graph.units, &graph.plans, &graph.packages, at, host(), &run) {
            Ok(std) => std,
            Err(why) => {
                note(out, &name, &format!("its cfg: {why}"));
                continue;
            }
        };
        let mut env: Vec<(String, String), Global> = Vec::new_in(Global);
        env.extend(graph.plans[at].env.iter().cloned());
        env.extend(std.env.iter().cloned());
        env.sort();
        if let Some(pair) = env.windows(2).find(|pair| pair[0].0 == pair[1].0) {
            note(out, &name, &format!("`{}` is set to `{}` and to `{}`", pair[0].0, pair[0].1, pair[1].1));
            continue;
        }
        let Ok(edition) = unit.target.edition.parse::<Edition>() else {
            note(out, &name, &format!("edition `{}`", unit.target.edition));
            continue;
        };
        let disk = |named: &str| stdlib::cfg::on_disk(named, &run).map_err(|why| why.to_string());
        let root = match unit.target.src_path.to_str().ok_or_else(|| "its root is not UTF-8".to_owned()).and_then(disk) {
            Ok(root) => root,
            Err(why) => {
                note(out, &name, &why);
                continue;
            }
        };
        let package = graph.packages.iter().find(|it| it.id == unit.pkg_id);
        let dir = match package.map(|it| disk(&it.manifest_path)) {
            Some(Ok(manifest)) => manifest.parent().map_or_else(PathBuf::new, Path::to_path_buf),
            Some(Err(why)) => {
                note(out, &name, &why);
                continue;
            }
            None => {
                note(out, &name, &format!("the graph has no plan of its package {}", unit.pkg_id));
                continue;
            }
        };
        let Some(root_named) = root.to_str() else {
            note(out, &name, &format!("{}: not UTF-8", root.display()));
            continue;
        };
        let options = options_of(&std.lines);
        let input = Gather {
            tool: TOOL,
            reading: Reading { edition, extensions: Extensions::NONE },
            cfg: &options,
            root: root_named.as_bytes(),
            env: &env,
            pair,
            member: false,
            defer: true,
        };
        let text = match gather::gather::<_, _, CensusDigests, _, _>(&input, &mut Disk, &mut images, Global) {
            Ok(text) => text,
            Err(error) => {
                let why = format!(
                    "gathering: {}: {} ({})",
                    String::from_utf8_lossy(&error.path),
                    String::from_utf8_lossy(&error.what),
                    error.rule
                );
                note(out, &name, &why);
                continue;
            }
        };
        let mut crate_files: Vec<(PathBuf, Vec<u8>), Global> = Vec::new_in(Global);
        let as_path = |path: &[u8]| PathBuf::from(String::from_utf8_lossy(path).into_owned());
        crate_files.extend(text.files.iter().map(|file| (as_path(&file.path), file.text.to_vec())));
        crate_files.extend(text.included.iter().map(|file| (as_path(&file.path), file.text.to_vec())));
        let mut deps: Vec<Dep, Global> = Vec::new_in(Global);
        for dep in unit.dependencies.iter() {
            if let Some(on) = graph.units.get(dep.index).filter(|it| it.target.is_lib()) {
                let marks = dep.marks(unit, on);
                deps.push(Dep { name: dep.extern_crate_name.clone(), unit: dep.index, marks });
            }
        }
        let lang = lang_of(&name);
        gathered.push((at, StdCrate { name, edition, lang, cfg: options, env, dir, deps, files: crate_files }));
    }

    // Dependency order: each crate once every crate it depends on is placed. A crate whose
    // dependency was left out above is never placed, and is noted.
    let mut placed: Vec<usize, Global> = Vec::new_in(Global);
    let mut order: Vec<usize, Global> = Vec::new_in(Global);
    loop {
        let before = order.len();
        for (index, (at, krate)) in gathered.iter().enumerate() {
            if placed.contains(at) {
                continue;
            }
            if krate.deps.iter().all(|dep| placed.contains(&dep.unit)) {
                placed.push(*at);
                order.push(index);
            }
        }
        if order.len() == before {
            break;
        }
    }
    for (at, krate) in gathered.iter() {
        if !placed.contains(at) {
            note(out, &krate.name, "not built: a crate it depends on is not");
        }
    }
    // The edges by census index, the crates in that order.
    let census_index = |unit: usize| placed.iter().position(|&it| it == unit).expect("a placed crate's dependency is placed");
    let mut taken: Vec<Option<(usize, StdCrate)>, Global> = Vec::with_capacity_in(gathered.len(), Global);
    taken.extend(gathered.into_iter().map(Some));
    let mut crates: Vec<StdCrate, Global> = Vec::with_capacity_in(order.len(), Global);
    for index in order {
        let (_, mut krate) = taken[index].take().expect("each crate placed once");
        for dep in krate.deps.iter_mut() {
            dep.unit = census_index(dep.unit);
        }
        crates.push(krate);
    }
    crates
}

/// One crate put in a database: its index in the census's crates and its files, the root first.
struct Given {
    at: usize,
    files: Vec<(PathBuf, Vec<u8>), Global>,
}

/// The sealed database of every crate of `given`, each in a library source root of its own
/// with every file it gathered, and each crate's key; a crate whose root has no text, or one
/// of whose dependencies is not built, is noted into `out` and left out.
fn database(crates: &[StdCrate], given: &[Given], out: &mut String) -> (Db, Vec<(usize, CrateKey), Global>) {
    // Sealed, as the command's snapshot is: a `mod` whose file was not gathered is refused at
    // its declaration, never read from disk.
    let mut db = Db::for_tests_sealed(|_| {});
    let workspace = workspace();
    let mut edit = db.edit();
    let mut library_roots = HashSet::new_in(Global);
    let mut roots: Vec<(usize, FileId), Global> = Vec::new_in(Global);
    for (index, crate_files) in given.iter().enumerate() {
        let krate = &crates[crate_files.at];
        let root_id = SourceRootId(index as u32);
        let mut set = FileSet::default();
        let mut root = None;
        for (number, (path, bytes)) in crate_files.files.iter().enumerate() {
            // The command's snapshot refuses a file that is not text (`snapshot::NOT_TEXT`).
            let Ok(text) = core::str::from_utf8(bytes) else {
                note(out, &krate.name, &format!("gathering: {}: not UTF-8, which a snapshot refuses", path.display()));
                continue;
            };
            let shown = path.to_str().unwrap_or_else(|| panic!("{}: its path is not UTF-8", path.display()));
            let absolute =
                AbsPathBuf::try_from(shown).unwrap_or_else(|_| panic!("{shown}: not absolute, under an absolute library"));
            let vfs = VfsPath::from(absolute);
            // A path gathered twice (a module file a late read includes too) is one file, as
            // the command's `snapshot::put_file` keeps one id a path.
            if set.file_for_path(&vfs).is_some() {
                continue;
            }
            let file = edit.new_file_id().expect("the database has file ids left");
            edit.set_file_text(file, text);
            edit.set_file_source_root(file, root_id);
            set.insert(file, vfs);
            if number == 0 {
                root = Some(file);
            }
        }
        edit.set_source_root(root_id, triomphe::Arc::new(SourceRoot::new_library(set)));
        library_roots.insert(root_id);
        match root {
            Some(root) => roots.push((crate_files.at, root)),
            None => note(out, &krate.name, "not built: its root has no text"),
        }
    }
    edit.set_local_roots(HashSet::new_in(Global));
    edit.set_library_roots(library_roots);
    let mut builder = CrateGraphBuilder::default();
    let mut built: Vec<(usize, CrateBuilderId), Global> = Vec::new_in(Global);
    let built_as = |built: &[(usize, CrateBuilderId)], at: usize| built.iter().find(|&&(it, _)| it == at).map(|&(_, id)| id);
    for &(at, root) in roots.iter() {
        let krate = &crates[at];
        if let Some(dep) = krate.deps.iter().find(|dep| built_as(&built, dep.unit).is_none()) {
            note(out, &krate.name, &format!("not built: its dependency `{}` is not", dep.name));
            continue;
        }
        let dir = krate.dir.to_str().unwrap_or_else(|| panic!("{}: its path is not UTF-8", krate.dir.display()));
        let cwd = triomphe::Arc::new(AbsPathBuf::try_from(dir).unwrap_or_else(|_| panic!("{dir}: not absolute")));
        let env: Env = krate.env.iter().cloned().collect();
        let id = builder.add_crate_root(
            root,
            krate.edition,
            Some(CrateDisplayName::from_canonical_name(&krate.name)),
            None,
            krate.cfg.clone(),
            None,
            env,
            CrateOrigin::Lang(krate.lang),
            Vec::new_in(Global),
            TargetKind::Lib { is_proc_macro: false },
            cwd,
            triomphe::Arc::clone(&workspace),
        );
        for dep in krate.deps.iter() {
            let to = built_as(&built, dep.unit).expect("every dependency is built, checked above");
            let name = CrateName::new(&dep.name).unwrap_or_else(|_| panic!("`{}` is no crate name", dep.name));
            builder
                .add_dep(id, DependencyBuilder::with_prelude(name, to, dep.marks.prelude, dep.marks.sysroot))
                .unwrap_or_else(|_| panic!("`{}`: a dependency cycle", krate.name));
        }
        built.push((at, id));
    }
    edit.set_crate_graph(builder);
    let mut keys = Vec::with_capacity_in(built.len(), Global);
    for &(at, id) in built.iter() {
        keys.push((at, edit.crate_key(id)));
    }
    // No process variable is read: a late `env!` sees the crate's env alone, as in a snapshot.
    edit.set_process_env(core::iter::empty::<(String, String)>().collect());
    _ = edit.commit();
    (db, keys)
}

/// The invariant: every library crate of the pinned std has a surface ([`image_parts`] refuses
/// none), gathered and given as the command gives a surface task its crate, and reads its own
/// def map back from that surface in a body snapshot ([`read_back`]). Every refusal of every
/// crate is listed in the one failure: by crate, kind and its text, which names the file and
/// line of the declaration or call it is about.
///
/// The control is `tests::a_census_lists_every_refusal_where_a_run_ends_at_the_first`: with
/// the census taken, two refusals of one crate are both listed.
#[test]
fn every_crate_of_the_pinned_std_has_a_surface() {
    let _turn = super::tests::TURN.lock();
    let library = library();
    let mut refused = String::new();
    let mut crates = std_crates(&library, &mut refused);
    let mut given = Vec::new_in(Global);
    for (at, krate) in crates.iter_mut().enumerate() {
        given.push(Given { at, files: core::mem::take(&mut krate.files) });
    }
    let (db, keys) = database(&crates, &given, &mut refused);
    let mut clean = 0;
    let mut surfaced: Vec<Surfaced, Global> = Vec::new_in(Global);
    for &(at, key) in keys.iter() {
        let name = crates[at].name.as_str();
        let Some(id) = db.crate_at(key) else {
            note(&mut refused, name, "committed, yet not in the database");
            continue;
        };
        let before = refused.len();
        let run = catch_unwind(AssertUnwindSafe(|| census::take(|| image_parts(&db, id))));
        match run {
            Ok((ended, met)) => {
                for why in met.iter() {
                    note(&mut refused, name, &format!("{}: {why}", why.kind()));
                }
                match ended {
                    Err(why) => note(&mut refused, name, &format!("{} (ended the run): {why}", why.kind())),
                    // A surface made with no refusal: its record's two parts the body snapshot
                    // reads, made as the surface task makes them (`blaze_front::surface`).
                    Ok(parts) if met.is_empty() => {
                        let address = ContentId::of("hir-def std census image", name.as_bytes());
                        let image = super::write_image(address, parts.parts.each_ref());
                        match catch_unwind(AssertUnwindSafe(|| crate::nameres::frozen::pages::encode(&db, id))) {
                            Ok(Ok(pages)) => surfaced.push(Surfaced { at, address, image, pages }),
                            Ok(Err(why)) => note(&mut refused, name, &format!("its pages refused: {why}")),
                            Err(payload) => {
                                note(&mut refused, name, &format!("its pages panicked: {}", panicked(&*payload)))
                            }
                        }
                    }
                    Ok(_) => {}
                }
            }
            Err(payload) => note(&mut refused, name, &format!("panicked: {}", panicked(&*payload))),
        }
        clean += usize::from(refused.len() == before);
    }
    let read_back = read_back(&crates, &given, &surfaced, &mut refused);
    assert!(
        refused.is_empty(),
        "{clean} of {} library crates of std at {} have a surface with no refusal, and {read_back} of the {} surfaced read their own def map back in a body snapshot; refused:\n{refused}",
        crates.len(),
        library.display(),
        surfaced.len()
    );
}

/// A crate's surface as the first pass made it: its image record, filed at `address`, and its
/// def map pages, each held whole, by the crate's index in the census's crates.
struct Surfaced {
    at: usize,
    address: ContentId,
    image: Vec<u8>,
    pages: Vec<u8>,
}

/// What a caught panic was: name resolution's typed stop said whole
/// ([`crate::nameres::stopped`], what the command reads off a task's payload), else its text.
// dyn: a caught panic's payload, read once
fn panicked(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(stopped) = crate::nameres::stopped(payload) {
        return stopped;
    }
    payload
        .downcast_ref::<&str>()
        .map(|it| (*it).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic with another payload".to_owned())
}

/// The second pass: every crate of `surfaced` given by its address in one new sealed database,
/// as `blaze_command::snapshot::open` opens a body task's snapshot. Each crate's root alone is
/// a file of its source root (edit 1); every crate is given its image and pages by
/// [`super::give_keyed`] in one edit (edit 2); each file its pages name is given the text of
/// the file it gathered under that name (edit 3, the command's `given_texts`). Then each
/// crate's own def map is read back: its head, and every module's scope, the reads a body
/// task's first queries make. Every refusal or panic is noted into `out` by crate with its
/// reason ([`panicked`], which says an `Unreadable` whole). The count of crates read back
/// clean.
fn read_back(crates: &[StdCrate], given: &[Given], surfaced: &[Surfaced], out: &mut String) -> usize {
    use crate::nameres::frozen::pages::{PageSource, Paged, Whole};
    // Edit 1: each surfaced crate with its root file alone.
    let mut roots: Vec<Given, Global> = Vec::new_in(Global);
    for made in surfaced {
        let Some(gathered) = given.iter().find(|it| it.at == made.at) else { continue };
        let Some(root) = gathered.files.first() else { continue };
        let mut files = Vec::with_capacity_in(1, Global);
        files.push(root.clone()); // clone: the root's path and text, once a crate
        roots.push(Given { at: made.at, files });
    }
    let mut setup = String::new();
    let (mut db, keys) = database(crates, &roots, &mut setup);
    for line in setup.lines() {
        // alloc: a census line, once a refusal
        out.push_str(&format!("read back: {line}\n"));
    }
    let in_db: Vec<(usize, base_db::Crate), Global> = {
        let mut in_db = Vec::with_capacity_in(keys.len(), Global);
        in_db.extend(keys.iter().filter_map(|&(at, key)| db.crate_at(key).map(|krate| (at, krate))));
        in_db
    };

    // Edit 2: every crate given by its address, deps first (`keys` is in dependency order).
    let mut given_ok: Vec<(usize, CrateKey), Global> = Vec::new_in(Global);
    {
        let mut edit = db.edit();
        for &(at, krate) in in_db.iter() {
            let name = crates[at].name.as_str();
            let made = surfaced.iter().find(|it| it.at == at).expect("a crate of the database was surfaced");
            let holder: super::ImageBuf = std::sync::Arc::new(Aligned::copy_from(&made.image));
            let pages_holder: super::ImageBuf = std::sync::Arc::new(Aligned::copy_from(&made.pages));
            let paged = Paged {
                key: ContentId::of("hir-def std census pages", &made.address.0),
                // dyn: `Paged::source` is a trait object
                source: std::sync::Arc::new(Whole(pages_holder)) as std::sync::Arc<dyn PageSource>,
            };
            let range = 0..made.image.len();
            let image =
                match super::Image::in_place(made.address, made.address, holder, range, super::Mode::Serve, Some(paged)) {
                    Ok(image) => image,
                    Err(why) => {
                        note(out, name, &format!("read back: its image does not open: {why}"));
                        continue;
                    }
                };
            let krate_id = ContentId::of("hir-def std census crate", name.as_bytes());
            #[expect(
                clippy::disallowed_methods,
                reason = "the census stands in for the driver's checked `Table`: the record was made just above"
            )]
            let read = super::ReadAtAddress::vouched(made.address);
            match super::give_keyed(&mut edit, krate, krate_id, image, read) {
                Ok(()) => given_ok.push((at, keys.iter().find(|it| it.0 == at).expect("keyed above").1)),
                Err(why) => note(out, name, &format!("read back: not given by its address: {why}")),
            }
        }
        _ = edit.commit();
    }

    // Edit 3: each file the pages name, the text of the file gathered under that name. The
    // `Crate` ids are read again: the give's commit made every given crate's arena again.
    let mut texts: Vec<(FileId, String), Global> = Vec::new_in(Global);
    for &(at, key) in given_ok.iter() {
        let name = crates[at].name.as_str();
        let Some(krate) = db.crate_at(key) else { continue };
        let Some(keyed) = super::keyed_files(&db, krate) else {
            note(out, name, "read back: given, yet has no keyed files");
            continue;
        };
        let gathered = given.iter().find(|it| it.at == at).expect("a given crate was gathered");
        let Some(dir) = gathered.files.first().and_then(|(root, _)| root.parent()).and_then(Path::to_str) else {
            continue;
        };
        for (path, bytes) in gathered.files.iter().skip(1) {
            let (Some(path), Ok(text)) = (path.to_str(), core::str::from_utf8(bytes)) else { continue };
            let named = crate::nameres::frozen::relative(dir, path);
            if let Ok(found) = keyed.binary_search_by(|it| (*it.path).cmp(&*named)) {
                texts.push((keyed[found].file, text.to_owned()));
            }
        }
    }
    {
        let mut edit = db.edit();
        for (file, text) in texts.iter() {
            edit.set_file_text(*file, text);
        }
        _ = edit.commit();
    }

    // Each crate's own def map, read back.
    let mut clean = 0;
    for &(at, key) in given_ok.iter() {
        let name = crates[at].name.as_str();
        let Some(krate) = db.crate_at(key) else {
            note(out, name, "read back: given, yet not in the database");
            continue;
        };
        let read = catch_unwind(AssertUnwindSafe(|| {
            let def_map = crate::nameres::crate_def_map(&db, krate);
            for (module, _) in def_map.modules(&db) {
                _ = module.def_map(&db).module(&db, module).scope(&db);
            }
        }));
        match read {
            Ok(()) => clean += 1,
            Err(payload) => note(out, name, &format!("read back: its own def map: {}", panicked(&*payload))),
        }
    }
    clean
}
