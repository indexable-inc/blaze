//! A NATIVE build script's `links` dependencies (D112, brief N8, main's ruling of 2026-11-16):
//! where a producer's output tree is seen is a fact of the CONSUMING run, never of a record.
//!
//! A producer's record names each path-valued metadata output (`cargo::metadata=include=..`,
//! `root=..`) under one of two trees: its `OUT_DIR` tree (its outputs record's files) or the
//! tree it was served (its package). What a value means is its tree and its place in it;
//! [`super::resolve`] is THE one function that turns it into the text a run is given, by the
//! run's [`Sight`] of those trees:
//!
//! - a wasm run ([`Sight::Wasm`], [`super::view`]) sees them at virtual mounts of its sandbox;
//! - a native run ([`Sight::Native`], [`lay`] here) sees them at REAL folders under a private
//!   root of its own (`<work>/<rel>`): `<root>/<links>/out` and `<root>/<links>/package`, the
//!   same relative places inside. [`lay`] writes exactly the producers' recorded files there
//!   before the script starts, and the run grants those folders READ-ONLY (they go in
//!   [`crate::native_script::NativeScript::reads`]; the only folder written stays `OUT_DIR`).
//!
//! A value under neither tree is refused, naming the variable and the producer
//! ([`DepViewError::Outside`]); nothing is passed through as written (rule 3).
//!
//! What the native script prints back that names a laid folder is written as the wasm sight
//! names it ([`Laid::shown`]), so its record holds no path of this run (rule 25), the same bytes
//! a wasm run of the key would keep.
//!
//! Deterministic: the values are a function of the producers' records and `root`; the folders
//! are sorted, each once; their files are the records' bytes, in their sorted order.
//!
//! Floor: the producers' files are written once per run that is not answered by its record
//! (one pass over bytes the run reads anyway); a record hit lays nothing. Cloning from the
//! store where the volume allows is the next step down (the producers' bytes are held in
//! memory here, `DepProducer::out`, not as store files).

use core::{fmt, time::Duration};
use std::path::Path;

use blaze_build_script::{
    dep_paths::{DepMount, Sight},
    env::DepMetadata,
};
use blaze_own::{AString, AVec, Alloc};
use bstd::{fs::Durable, mem::TASK_FRAME};

use super::{DepProducer, DepViewError, resolve};
use crate::{
    native_script::{listed_under, remove},
    record::native_world,
    script_sandbox::Tree,
};

/// What a native run is given of its `links` dependencies.
#[derive(Debug)]
pub struct Laid<'a> {
    /// Each pair's value as the native run sees it, in the order of the `deps` given.
    pub values: AVec<'a, AString<'a>>,
    /// The folders laid, sorted, each once: granted read-only, and nothing else of the
    /// producers is.
    pub folders: AVec<'a, AString<'a>>,
    /// Each laid folder with the text the wasm sight gives it, for the run's output: what the
    /// script prints under a laid folder is kept as the record's form.
    pub shown: AVec<'a, (AString<'a>, AString<'a>)>,
}

/// Why a native run's `links` dependencies could not be laid; each names the thing.
#[derive(Debug)]
pub enum LayError {
    /// A value cannot be resolved (outside both trees, no producer, climbing out).
    View(DepViewError),
    /// A producer whose served tree is a workspace folder: a native run is laid fetched
    /// packages' trees only (not built: refused by name, rule 3).
    MemberTree { links: String },
    /// A `links` value that is not one plain path part, so it cannot name a folder.
    LinksName { links: String },
    /// A folder or file could not be made.
    Files { folder: String, why: String },
}

impl From<DepViewError> for LayError {
    fn from(error: DepViewError) -> LayError {
        LayError::View(error)
    }
}

impl fmt::Display for LayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LayError::View(error) => write!(f, "{error}"),
            LayError::MemberTree { links } => write!(
                f,
                "the `links = \"{links}\"` package is a workspace member whose metadata names its workspace: a native build script is given fetched packages' trees only (D112, brief N8; not built)"
            ),
            LayError::LinksName { links } => {
                write!(f, "`links = \"{links}\"` is not one plain path part, so its files cannot be laid for a native build script (D112, brief N8)")
            }
            LayError::Files { folder, why } => write!(f, "{folder}: laying a `links` dependency's files for a native build script: {why}"),
        }
    }
}

impl core::error::Error for LayError {}

/// Lays the trees `deps` name under `<work>/<rel>` and gives each pair's value as the native
/// run sees it. `work` is the command's canonical work folder; `rel` a relative folder of this
/// run's own (`deps/<run key>`), emptied first. `producers` are the reading script's `links`
/// producers, each with its `links`.
pub fn lay<'a>(work: &str, rel: &str, producers: &[(&str, &DepProducer)], deps: &[DepMetadata<'_>], alloc: Alloc<'a>) -> Result<Laid<'a>, LayError> {
    lay_with_area(work, rel, None, producers, deps, alloc)
}

/// Like [`lay`], also materializing stored `OUT_DIR` files from `file_area` (hardlinked,
/// falling back to copy) when `file_area` is `Some`.
pub fn lay_with_area<'a>(work: &str, rel: &str, file_area: Option<&Path>, producers: &[(&str, &DepProducer)], deps: &[DepMetadata<'_>], alloc: Alloc<'a>) -> Result<Laid<'a>, LayError> {
    let work = work.trim_end_matches('/');
    let rel = rel.trim_matches('/');
    let root = format!("{work}/{rel}");
    let resolved = resolve(Sight::Native { root: &root }, producers, deps, alloc)?;

    // Each folder once, sorted: (where the native run sees it, where the wasm sight does, its
    // inline files, its stored files). Two pairs under one tree share it.
    let mut places: Vec<(AString<'a>, AString<'a>, &[(String, Vec<u8>)], &[(String, [u8; 32])])> = Vec::with_capacity(resolved.serve.len());
    for &(mount, producer) in resolved.serve.iter() {
        let links = match mount {
            DepMount::Out { links, .. } | DepMount::Tree { links } => links,
        };
        if links.is_empty() || links == "." || links == ".." || links.contains('/') || links.contains('\\') {
            return Err(LayError::LinksName { links: links.to_owned() });
        }
        let (files, stored): (&[(String, Vec<u8>)], &[(String, [u8; 32])]) = match mount {
            DepMount::Out { .. } => {
                if let Some(file) = producer.stored.first()
                    && file_area.is_none()
                {
                    return Err(LayError::View(DepViewError::StoredOut { links: links.to_owned(), file: file.clone() }));
                }
                (producer.out.as_slice(), producer.stored_files.as_slice())
            }
            DepMount::Tree { .. } => match &producer.tree {
                Tree::Mem { files, .. } => (files.as_slice(), &[]),
                // The wasm sight serves a member's workspace as the reader's own tree; a
                // native run is not given a workspace folder (not built).
                Tree::Dir(_) => return Err(LayError::MemberTree { links: links.to_owned() }),
            },
        };
        let mut native = AString::new_in(alloc);
        Sight::Native { root: &root }.seen_at(mount, &mut native);
        let mut shown = AString::new_in(alloc);
        Sight::Wasm.seen_at(mount, &mut shown);
        places.push((native, shown, files, stored));
    }
    places.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    places.dedup_by(|a, b| a.0.as_str() == b.0.as_str());

    empty(&root)?;
    let (io, world) = native_world(Path::new(work)).map_err(|e| LayError::Files { folder: work.to_owned(), why: e.to_string() })?;
    let mut folders: AVec<'a, AString<'a>> = AVec::with_capacity_in(places.len(), alloc);
    let mut shown: AVec<'a, (AString<'a>, AString<'a>)> = AVec::with_capacity_in(places.len(), alloc);
    for (native, wasm, files, stored) in places {
        let folder = native.as_str();
        let failed = |why: String| LayError::Files { folder: folder.to_owned(), why };
        // Under `work` by construction (`root` is `<work>/<rel>`).
        let under = folder.strip_prefix(work).map(|r| r.trim_start_matches('/')).ok_or_else(|| failed("not under the work folder".to_owned()))?;
        let cx = io.cx(Duration::from_secs(60), TASK_FRAME).map_err(|_| failed("its world stopped".to_owned()))?;
        let empty_tree = files.is_empty() && stored.is_empty();
        // An empty tree is laid as an empty folder (a marker written, then removed).
        let marker: [(String, Vec<u8>); 1] = [(".blaze-made".to_owned(), Vec::new())];
        let all = if empty_tree { &marker[..] } else { files };
        for (name, bytes) in all {
            let at = format!("{under}/{name}");
            let path = bstd::path::Path::new(&at).map_err(|_| failed(format!("{name}: not a relative path")))?;
            let file = world.create(&cx, &path).map_err(|e| failed(format!("{name}: {e}")))?;
            file.write_at(&cx, 0, bytes, Durable::No).map_err(|e| failed(format!("{name}: {e}")))?;
            file.close(&cx, Durable::No).map_err(|e| failed(format!("{name}: {e}")))?;
        }
        if let Some(area) = file_area {
            for (name, digest) in stored {
                let mut hex = String::with_capacity(64);
                for b in digest {
                    use core::fmt::Write;
                    let _ = write!(&mut hex, "{b:02x}");
                }
                let src = area.join(&hex);
                let dst = Path::new(folder).join(name);
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| failed(format!("{name}: {e}")))?;
                }
                if std::fs::hard_link(&src, &dst).is_err() {
                    std::fs::copy(&src, &dst).map_err(|e| failed(format!("{name} (from {}): {e}", src.display())))?;
                }
            }
        }
        if empty_tree {
            remove(folder, ".blaze-made").map_err(|e| failed(e.to_string()))?;
        }
        folders.push(blaze_own::String::from_str_in(native.as_str(), alloc));
        shown.push((native, wasm));
    }
    Ok(Laid { values: resolved.values, folders, shown })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use blaze_build_script::outputs::{FileLimits, OutputsRef};
    use blaze_build_script::{Outputs, instr::OutputLimits};
    use blaze_manifest::lock::LockId;
    use blaze_own::AVec;
    use blaze_unit_graph::{Resolver, Text, Unit, UnitDep, UnitGraph, UnitId, UnitKind};

    use super::lay;
    use crate::script_sandbox::{ScriptRoot, Tree};
    use crate::script_unit::{GivenRun, UnitScriptError, served_links};

    const LZ4_OUT: &str = "/target/debug/build/lz4-sys-0123456789abcdef/out";
    const HEADER: &[u8] = b"#define LZ4_VERSION_MAJOR 1\n";

    fn t(s: &str) -> Text<'static> {
        Text::from_str_in(s, blaze_own::testing::heap().alloc())
    }

    fn unit(package: u32, name: &str, kind: UnitKind<'static>, deps: &[u32], build_script: Option<u32>, links: Option<&str>) -> Unit<'static> {
        let alloc = blaze_own::testing::heap().alloc();
        let mut d = AVec::new_in(alloc.clone());
        for u in deps {
            d.push(UnitDep { name: t("dep"), unit: UnitId(*u) });
        }
        Unit {
            package: LockId(package),
            name: t(name),
            version: t("1.0.0"),
            kind,
            for_host: false,
            edition: edition::PackageEdition::of_manifest("2021").expect("an edition"),
            crate_name: t(name),
            root: t("src/lib.rs"),
            features: AVec::new_in(alloc.clone()),
            cfg: AVec::new_in(alloc),
            deps: d,
            build_script: build_script.map(UnitId),
            links: links.map(t),
        }
    }

    /// librocksdb-sys (script 0, its library 1) depends on lz4-sys (`links = "lz4"`, script 2,
    /// its library 3), as in tempo's graph.
    fn graph() -> UnitGraph<'static> {
        let alloc = blaze_own::testing::heap().alloc();
        let mut units = AVec::new_in(alloc.clone());
        units.push(unit(0, "librocksdb-sys", UnitKind::BuildScript, &[], None, None));
        units.push(unit(0, "librocksdb-sys", UnitKind::Lib, &[3], Some(0), Some("rocksdb")));
        units.push(unit(1, "lz4-sys", UnitKind::BuildScript, &[], None, None));
        units.push(unit(1, "lz4-sys", UnitKind::Lib, &[], Some(2), Some("lz4")));
        UnitGraph { units, resolver: Resolver::V2, meta: AVec::new_in(alloc), left_out: AVec::new_in(alloc) }
    }

    /// Export-9's librocksdb-sys stop ("DEP_lz4_root: the `links = \"lz4\"` package's run was
    /// not given"): a NATIVE reader of a `links` dependency is given that dependency's run as its
    /// outputs RECORD through `script_unit::served_links`, the one function blaze-cli's `Script`
    /// task and tempo_scripts' export both call (`outputs_given`). The reader gets
    /// `DEP_LZ4_INCLUDE` and `DEP_LZ4_ROOT` from the record's metadata, and the laid folders hold
    /// lz4's `OUT_DIR` header and its package's `inc/b.h`, read back from disk.
    /// CONTROL (the cause removed: no run given, as the exporter's native path passed none): the
    /// reader is refused naming lz4-sys and `links = "lz4"`, never run without its pairs.
    /// Prints `dep_mount native: librocksdb-sys given lz4's record, lz4.h and b.h laid`.
    #[test]
    fn a_native_reader_is_given_its_links_dependency_s_record_and_reads_its_files() {
        let alloc = blaze_own::testing::heap().alloc();
        let graph = graph();
        let printed = format!("cargo:include={LZ4_OUT}/include\ncargo:root=/package/inc\n");
        let mut outputs = Outputs::read(printed.as_bytes(), LZ4_OUT, OutputLimits::DEFAULT, alloc.clone()).unwrap_or_else(|e| panic!("{e:?}"));
        outputs.add_file("include/lz4.h", HEADER, FileLimits::DEFAULT, alloc.clone()).unwrap_or_else(|e| panic!("{e:?}"));
        let mut record = AVec::new_in(alloc.clone());
        outputs.encode(&mut record);
        let opened = OutputsRef::open(&record).unwrap_or_else(|e| panic!("{e:?}"));
        let tree = Tree::Mem { files: Arc::new(vec![("inc/b.h".to_owned(), b"int b;\n".to_vec())]), git: None };
        let given = [GivenRun { script: UnitId(2), links: "lz4", outputs: opened, tree, root: ScriptRoot::package(), direct: true }];

        let served = served_links(&graph, UnitId(0), &given).unwrap_or_else(|e| panic!("{e}"));
        let pairs: Vec<(&str, &str, &str)> = served.deps.iter().map(|dep| (dep.links, dep.key, dep.value)).collect();
        let include = format!("{LZ4_OUT}/include");
        assert_eq!(pairs, [("lz4", "include", include.as_str()), ("lz4", "root", "/package/inc")], "DEP_LZ4_* from the record");
        let producers: Vec<(&str, &super::DepProducer)> = served.producers.iter().map(|(links, producer)| (*links, producer)).collect();
        assert_eq!(producers.iter().map(|(links, _)| *links).collect::<Vec<_>>(), ["lz4"]);

        let root = std::env::temp_dir().join(format!("blaze-xp-lay-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("the work folder");
        let work = root.canonicalize().expect("its real path");
        let work = work.to_str().expect("a UTF-8 temp path").to_owned();
        let laid = lay(&work, "deps/k", &producers, &served.deps, alloc.clone());
        let read = |path: String| std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let (header, b) = match &laid {
            Ok(laid) => (read(format!("{}/lz4.h", laid.values[0].as_str())), read(format!("{}/b.h", laid.values[1].as_str()))),
            Err(_) => (Vec::new(), Vec::new()),
        };
        _ = std::fs::remove_dir_all(&root);
        let laid = laid.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(laid.values[0].as_str(), format!("{work}/deps/k/lz4/out/include"));
        assert_eq!(header, HEADER, "lz4's OUT_DIR header, laid from its record");
        assert_eq!(b, b"int b;\n", "lz4's package file, laid from its served tree");

        match served_links(&graph, UnitId(0), &[]) {
            Err(UnitScriptError::Dependency { dependency, links, .. }) => assert_eq!((dependency.as_str(), links.as_str()), ("lz4-sys 1.0.0", "lz4")),
            Err(other) => panic!("refused otherwise: {other}"),
            Ok(_) => panic!("a reader was served without its `links` dependency's run"),
        }
        println!("dep_mount native: librocksdb-sys given lz4's record, lz4.h and b.h laid");
    }
}

/// `root` holding nothing: what an earlier run of the same key left there is removed.
fn empty(root: &str) -> Result<(), LayError> {
    if !Path::new(root).exists() {
        return Ok(());
    }
    let failed = |why: String| LayError::Files { folder: root.to_owned(), why };
    for left in listed_under(root).map_err(|e| failed(e.to_string()))? {
        remove(root, &left.name).map_err(|e| failed(e.to_string()))?;
    }
    Ok(())
}
