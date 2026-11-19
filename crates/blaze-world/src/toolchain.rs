//! The toolchain's crates a graph reaches, read from the shelf's verified packs: core and alloc
//! always, Rust's `std` for a Rust-edition crate, bstd for a blaze-edition one, `proc_macro`
//! for a proc-macro crate, and what each is built from by its row's edges, the sysroot crates
//! its root names among them (so std comes in iff a crate of the closure names it; target
//! side; a proc-macro unit's own dependencies are the host's, not followed).
//!
//! A crate's cfg is the target's atoms as the toolchain record states them, the profile's
//! (`debug_assertions`), and a `feature` atom per feature on.

use blaze_id::{ContentId, CrateItemKey};
use blaze_own::{AVec, Alloc};
use blaze_surface::Edition;
use blaze_toolchain::{Role, Shelf, ToolchainId, ToolchainView, UnitKind};
use blaze_unit_graph::{Implicit, implicit};
use edition::PackageEdition;

use crate::{WorldError, join, keep, keep_slice};

/// The profile's atoms beside the target's: `dev`.
const PROFILE: &[(&str, Option<&str>)] = &[("debug_assertions", None)];

/// INTERIM, until std's build script runs like any other (the toolchain cut storing its outputs
/// record on the shelf beside the packs): the cfg atoms `library/std/build.rs` (pinned nightly
/// 2026-10-27) prints for each target blaze builds for. Every print of that script, with its
/// condition:
/// - line 13 `cargo:rustc-cfg=netbsd10`: `target_os == "netbsd"` and `RUSTC_STD_NETBSD10` set;
///   no target below.
/// - line 76 `cargo:rustc-cfg=restricted_std`: a `target_os` outside the list at lines 20..33
///   (which holds "linux", line 20, and "macos", line 28); no target below.
/// - line 80 `cargo:rustc-cfg=backtrace_in_libstd`: always; every target below.
/// - line 82 `cargo:rustc-env=STD_ENV_ARCH=<CARGO_CFG_TARGET_ARCH>`: always, the target's arch;
///   not carried: a toolchain crate holds no environment, and no item std writes is decided by it.
/// - line 89 `cargo:rustc-cfg=vxworks_lt_25_09`: `target_os == "vxworks"`; no target below.
const STD_SCRIPT_CFG: &[(&str, &[&str])] = &[
    // build.rs:80 `println!("cargo:rustc-cfg=backtrace_in_libstd");`
    ("aarch64-apple-darwin", &["backtrace_in_libstd"]),
    // build.rs:80 `println!("cargo:rustc-cfg=backtrace_in_libstd");`
    ("aarch64-unknown-linux-gnu", &["backtrace_in_libstd"]),
    // build.rs:80 `println!("cargo:rustc-cfg=backtrace_in_libstd");`
    ("x86_64-apple-darwin", &["backtrace_in_libstd"]),
    // build.rs:80 `println!("cargo:rustc-cfg=backtrace_in_libstd");`
    ("x86_64-unknown-linux-gnu", &["backtrace_in_libstd"]),
];

/// std's build-script cfg for `triple` ([`STD_SCRIPT_CFG`]); a target with no row is refused.
fn std_script_cfg<'w>(triple: &'w str) -> Result<&'static [&'static str], WorldError<'w>> {
    STD_SCRIPT_CFG.iter().find(|(t, _)| *t == triple).map(|(_, atoms)| *atoms).ok_or(WorldError::NoStdScriptCfg { triple })
}

/// Which toolchain crates the PROGRAM's own units need beyond core and alloc
/// ([`crate::needs`]): Rust's std for a unit given it, bstd for a blaze-edition one,
/// `proc_macro` for a proc macro. A toolchain crate's own need is its row's edges
/// (blaze_toolchain::implied), followed by [`Toolchain::open`]'s walk: so a crate of bstd's
/// closure that uses std's prelude brings std in, and a closure of `no_std` crates does not.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Needs {
    pub std: bool,
    pub bstd: bool,
    pub proc_macro: bool,
}

/// One toolchain crate: its input's parts, owned by the allocator the toolchain was opened in.
#[derive(Clone, Copy, Debug)]
pub struct ToolchainCrate<'w> {
    pub name: &'w str,
    pub version: &'w str,
    /// Its unit's index in the manifest's unit list for the triple.
    pub unit: u32,
    pub key: CrateItemKey,
    pub edition: Edition,
    pub implicit: Implicit,
    /// What its root's text gives it from the sysroot, as the cutter read it (its unit row's
    /// `sysroot`); the crates it names are among [`ToolchainCrate::deps`].
    pub sysroot: blaze_toolchain::ImplicitSysroot,
    pub root: &'w str,
    pub shown_from: &'w str,
    pub features: &'w [&'w str],
    /// By index among the toolchain's crates (which are the world's first crates).
    pub deps: &'w [(&'w str, usize)],
    pub cfg: &'w [(&'w str, Option<&'w str>)],
    pub proc_macro: bool,
    role: Role,
    pack: usize,
}

/// The toolchain crates a graph needs, their packs read and verified once.
pub struct Toolchain<'w, B> {
    crates: AVec<'w, ToolchainCrate<'w>>,
    packs: AVec<'w, B>,
}

fn edition_of(written: &str) -> Option<Edition> {
    Some(match written {
        "2015" => Edition::E2015,
        "2018" => Edition::E2018,
        "2021" => Edition::E2021,
        "2024" => Edition::E2024,
        _ => return None,
    })
}

impl<'w, B: AsRef<[u8]>> Toolchain<'w, B> {
    /// Opens toolchain `id`'s manifest on `shelf` and reads the pack of every unit `needs`
    /// reaches for `triple`.
    pub fn open<S: Shelf<Bytes = B>>(shelf: &mut S, id: ToolchainId, triple: &'w str, needs: Needs, alloc: Alloc<'w>) -> Result<Toolchain<'w, B>, WorldError<'w>> {
        let manifest = blaze_toolchain::shelf::open_manifest(shelf, id).map_err(WorldError::Toolchain)?;
        let view = ToolchainView::open(manifest.as_ref(), id).expect("open_manifest checked it");
        let sys = blaze_toolchain::sysroot(&view, triple, false).ok_or(WorldError::NoTarget { triple })?;
        let all: AVec<'w, _> = {
            let mut v = AVec::new_in(alloc.clone());
            v.extend(view.units(triple).ok_or(WorldError::NoTarget { triple })?);
            v
        };
        let mut keep_unit: AVec<'w, bool> = AVec::with_capacity_in(all.len(), alloc.clone());
        keep_unit.extend(all.iter().map(|_| false));
        let mut todo: AVec<'w, usize> = AVec::new_in(alloc.clone());
        // core and alloc; std for the program's own need. Every other toolchain crate comes in
        // by an edge: a row's deps hold the sysroot crates its root names (the cutter's
        // implied edges), so std is opened iff a crate of the closure names it.
        todo.extend([sys.core.0 as usize, sys.alloc.0 as usize]);
        if needs.std {
            todo.push(sys.std.0 as usize);
        }
        if needs.bstd {
            todo.push(sys.bstd.0 as usize);
        }
        if needs.proc_macro {
            todo.push(sys.proc_macro.0 as usize);
        }
        while let Some(u) = todo.pop() {
            if keep_unit[u] {
                continue;
            }
            keep_unit[u] = true;
            if all[u].kind != UnitKind::ProcMacro {
                todo.extend(all[u].deps().map(|(_, d)| d as usize));
            }
        }
        let mut at: AVec<'w, usize> = AVec::with_capacity_in(all.len(), alloc.clone());
        at.extend(all.iter().map(|_| usize::MAX));
        let mut crates: AVec<'w, ToolchainCrate<'w>> = AVec::new_in(alloc.clone());
        let mut packs: AVec<'w, B> = AVec::new_in(alloc.clone());
        let cfg_target: AVec<'w, (&'w str, Option<&'w str>)> = {
            let mut v = AVec::new_in(alloc.clone());
            for (name, value) in view.cfg(triple).ok_or(WorldError::NoTarget { triple })? {
                v.push((keep(name, alloc.clone()), value.map(|v| keep(v, alloc.clone()))));
            }
            v
        };
        // First the rows, then their dependencies by index among the kept.
        let mut raw_deps: AVec<'w, AVec<'w, (&'w str, usize)>> = AVec::new_in(alloc.clone());
        for (u, row) in all.iter().enumerate() {
            let proc_macro = row.kind == UnitKind::ProcMacro;
            if !keep_unit[u] || !(proc_macro || (row.kind == UnitKind::Lib && !row.for_host)) {
                continue;
            }
            let krate = view.krate(row.krate);
            let name = keep(krate.lib, alloc.clone());
            let edition = edition_of(krate.edition).ok_or(WorldError::ToolchainEdition { krate: name, written: keep(krate.edition, alloc.clone()) })?;
            // Its implicit is its own, from its row: its crate's edition (the prelude's
            // `rust_20xx`), never one uniform Rust 2024; what it is given from the sysroot is
            // its row's value and edges (`row.sysroot`, `row.deps()`).
            let own = PackageEdition::of_manifest(krate.edition).map_err(|_| WorldError::ToolchainEdition { krate: name, written: keep(krate.edition, alloc.clone()) })?;
            let own_implicit = implicit(own, if proc_macro { &blaze_unit_graph::UnitKind::ProcMacro } else { &blaze_unit_graph::UnitKind::Lib });
            let pack = blaze_toolchain::shelf::open_pack(shelf, id, &krate).map_err(WorldError::Toolchain)?;
            let index = crates.len();
            let mut base = blaze_own::AString::with_capacity_in(16, alloc.clone());
            base.push_str("/");
            crate::push_u32(&mut base, index as u32 + 1);
            base.push_str("/");
            let root = if krate.dir.is_empty() || row.root.starts_with(krate.dir) {
                join(&base, row.root, alloc.clone())
            } else {
                join(&join(&base, krate.dir, alloc.clone()), row.root, alloc.clone())
            };
            let mut features: AVec<'w, &'w str> = AVec::new_in(alloc.clone());
            features.extend(row.features().map(|f| keep(f, alloc.clone())));
            let mut cfg: AVec<'w, (&'w str, Option<&'w str>)> = AVec::with_capacity_in(cfg_target.len() + features.len() + 1, alloc.clone());
            cfg.extend_from_slice(&cfg_target);
            cfg.extend_from_slice(PROFILE);
            cfg.extend(features.iter().map(|f| ("feature", Some(*f))));
            let script = if krate.role == Role::Std { std_script_cfg(triple)? } else { &[] };
            cfg.extend(script.iter().map(|atom| (*atom, None)));
            let mut key_bytes: AVec<'w, u8> = AVec::new_in(alloc.clone());
            key_bytes.extend_from_slice(&krate.pack);
            for f in row.features() {
                key_bytes.extend_from_slice(f.as_bytes());
                key_bytes.push(0);
            }
            key_bytes.extend_from_slice(b"\0script cfg");
            for atom in script {
                key_bytes.extend_from_slice(atom.as_bytes());
                key_bytes.push(0);
            }
            let mut deps: AVec<'w, (&'w str, usize)> = AVec::new_in(alloc.clone());
            if !proc_macro {
                deps.extend(row.deps().map(|(n, d)| (keep(n, alloc.clone()), d as usize)));
            }
            raw_deps.push(deps);
            at[u] = index;
            crates.push(ToolchainCrate {
                name,
                version: keep(krate.version, alloc.clone()),
                unit: u as u32,
                key: CrateItemKey(ContentId::of("blaze toolchain unit", &key_bytes)),
                edition,
                implicit: own_implicit,
                sysroot: row.sysroot,
                root: keep(&root, alloc.clone()),
                shown_from: keep(&base, alloc.clone()),
                features: features.leak(),
                deps: &[],
                cfg: cfg.leak(),
                proc_macro,
                role: krate.role,
                pack: packs.len(),
            });
            packs.push(pack);
        }
        for (c, deps) in crates.iter_mut().zip(raw_deps.iter_mut()) {
            for dep in deps.iter_mut() {
                let kept = at[dep.1];
                if kept == usize::MAX {
                    return Err(WorldError::ToolchainDependencyNotKept { krate: c.name, dep: dep.0 });
                }
                dep.1 = kept;
            }
            c.deps = keep_slice(deps, alloc.clone());
        }
        // A crate's key is its identity: its pack's digest and features (set above), the
        // target's cfg (the same pack is another crate on another target), and its dependencies'
        // keys by name. Dependencies first, by passes over the kept crates (no recursion): a
        // toolchain is a DAG, so each pass keys at least one crate; past `crates.len()` passes
        // a dependency cycle is a typed refusal.
        let mut keyed: AVec<'w, bool> = AVec::with_capacity_in(crates.len(), alloc.clone());
        keyed.extend(crates.iter().map(|_| false));
        let mut left = crates.len();
        let mut passes = 0;
        while left > 0 {
            passes += 1;
            if passes > crates.len() {
                let c = keyed.iter().position(|k| !k).map_or("", |c| crates[c].name);
                return Err(WorldError::ToolchainCycle { krate: c });
            }
            for c in 0..crates.len() {
                if keyed[c] || crates[c].deps.iter().any(|(_, d)| !keyed[*d]) {
                    continue;
                }
                let mut bytes: AVec<'_, u8> = AVec::new_in(alloc.clone());
                bytes.extend_from_slice(&crates[c].key.0.0);
                bytes.extend_from_slice(b"\0cfg");
                for (name, value) in cfg_target.iter() {
                    bytes.extend_from_slice(name.as_bytes());
                    bytes.push(0);
                    bytes.extend_from_slice(value.unwrap_or("\u{0}none").as_bytes());
                    bytes.push(0);
                }
                bytes.extend_from_slice(b"\0deps");
                for (name, d) in crates[c].deps.iter() {
                    bytes.extend_from_slice(name.as_bytes());
                    bytes.push(0);
                    bytes.extend_from_slice(&crates[*d].key.0.0);
                }
                crates[c].key = CrateItemKey(ContentId::of("blaze toolchain crate 2", &bytes));
                keyed[c] = true;
                left -= 1;
            }
        }
        let tc = Toolchain { crates, packs };
        if tc.core().is_none() {
            return Err(WorldError::NoToolchainCrate { role: "core" });
        }
        if needs.std && tc.std().is_none() {
            return Err(WorldError::NoToolchainCrate { role: "std" });
        }
        if needs.bstd && tc.bstd().is_none() {
            return Err(WorldError::NoToolchainCrate { role: "bstd" });
        }
        if needs.proc_macro && tc.proc_macro().is_none() {
            return Err(WorldError::NoToolchainCrate { role: "proc_macro" });
        }
        Ok(tc)
    }
}

impl<'w, B: AsRef<[u8]>> Toolchain<'w, B> {
    pub fn crates(&self) -> &[ToolchainCrate<'w>] {
        &self.crates
    }

    /// The verified pack bytes `c` is read from.
    pub fn pack(&self, c: &ToolchainCrate<'_>) -> &[u8] {
        self.packs[c.pack].as_ref()
    }

    fn role(&self, role: Role) -> Option<usize> {
        self.crates.iter().position(|c| c.role == role && !c.proc_macro)
    }

    pub fn core(&self) -> Option<usize> {
        self.role(Role::Core)
    }

    pub fn alloc(&self) -> Option<usize> {
        self.role(Role::Alloc)
    }

    pub fn std(&self) -> Option<usize> {
        self.role(Role::Std)
    }

    pub fn bstd(&self) -> Option<usize> {
        self.role(Role::Bstd)
    }

    pub fn proc_macro(&self) -> Option<usize> {
        self.role(Role::ProcMacro)
    }
}
