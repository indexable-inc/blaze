//! The trust lints' facts, per crate and per function, each a tracked query keyed by one crate
//! as `weight::rows`' `body_uses` and `crate_uses` are:
//!
//! - [`reached`]: what one workspace crate's functions reach in the dependencies, each
//!   (crate, capability) once with its shortest witness ([`attribute`] over
//!   [`hir_ty_pure::trust::reach`]). Each instance's own facts are hir-ty-pure's per-instance
//!   queries (`edge`, `toolchain_call`), read off the walk's summaries: an edit of a workspace
//!   body reruns that body's summary and this crate's breadth-first pass over the cached edges,
//!   never a dependency's.
//! - [`anywhere`]: what a dependency's items say wherever they are, reached or not (`unsafe`,
//!   foreign declarations, symbol attributes), and whether it is a proc macro.
//! - [`unreached`]: what each of a dependency's functions does itself, whether the workspace
//!   reaches it or not: the walk of every function of the crate, once per crate.
//!
//! [`workspace`] folds them into one [`Workspace`]: each dependency package (by name and
//! version) with what it uses, each capability with one witness, sorted.

use std::alloc::Global;

use base_db::{Crate, CrateOrigin, Db, TargetKind};
use blaze_hash::HashMap;
use hir_def::{
    AttrDefId, DefWithBodyId, FunctionId, ItemContainerId, ModuleDefId,
    attrs::AttrFlags,
    expr_store::Body,
    hir::{Expr, Unsafe},
    nameres::crate_def_map,
    signatures::{
        FunctionSignature, ImplFlags, ImplSignature, StaticFlags, StaticSignature, TraitFlags,
        TraitSignature,
    },
};
use hir_ty_pure::trust::{Done, Reach, function_path, reach, toolchain_crate};

use super::{
    attribute::{Graph, Node, attribute},
    capability::{ANYWHERE, Capability, Caps, IO_SPLIT, UNSEEN_STANDS_FOR, of_done},
    grants::order,
};
use crate::check;

/// The build-script dependencies that build or link foreign code: a package whose build script
/// depends on one of them is counted [`Capability::Ffi`].
pub const FOREIGN_BUILDERS: &[&str] = &["cc", "cmake", "bindgen", "pkg_config", "vcpkg"];

/// What the workspace's dependencies use.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Workspace {
    /// Each dependency package, by name then version.
    pub deps: Box<[Dep]>,
}

/// One dependency package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dep {
    /// Its package name.
    pub name: Box<str>,
    pub version: Option<Box<str>>,
    /// What it uses where the workspace reaches it, and anywhere for [`ANYWHERE`]'s and its
    /// foreign declarations; in [`Capability::ALL`]'s order.
    pub uses: Box<[Used]>,
    /// What its code does that nothing the workspace builds reaches, and is not in `uses`.
    pub unreached: Box<[Used]>,
}

/// One capability a dependency uses, with one witness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Used {
    pub capability: Capability,
    /// The calls from the workspace to where it is used and what is done there:
    /// "`main::run` → `left_pad::phone_home` calls `std::net::TcpStream::connect`".
    pub witness: Box<str>,
}

impl Dep {
    /// Whether it uses `capability`, reached or anywhere.
    pub fn uses(&self, capability: Capability) -> bool {
        self.uses.iter().any(|it| it.capability == capability)
    }

    /// Its version as a finding says it: `1.4.0`, or nothing.
    pub fn shown_version(&self) -> &str {
        self.version.as_deref().unwrap_or("")
    }
}

/// The crate the trust lints report the workspace's findings at, once: the first workspace
/// library or binary by name, a library before a binary of the same name.
pub fn representative(db: &Db) -> Option<Crate> {
    let rank = |kind: TargetKind| match kind {
        TargetKind::Lib { .. } => Some(0u8),
        TargetKind::Bin => Some(1),
        TargetKind::Example
        | TargetKind::Test
        | TargetKind::Bench
        | TargetKind::BuildScript
        | TargetKind::Other => None,
    };
    let mut best: Option<(u8, Crate)> = None;
    for &krate in base_db::all_crates(db).iter() {
        let data = krate.data(db);
        if !data.origin.is_local() {
            continue;
        }
        let Some(kind) = rank(data.target_kind) else { continue };
        let better = match best {
            None => true,
            Some((best_kind, best)) => {
                (crate_name(db, krate), kind) < (crate_name(db, best), best_kind)
            }
        };
        if better {
            best = Some((kind, krate));
        }
    }
    best.map(|(_, krate)| krate)
}

/// What the workspace's dependencies use: the folds of [`reached`] over every workspace crate,
/// of [`anywhere`] and [`unreached`] over every dependency crate.
///
/// Caller: the trust lints at the workspace's [`representative`], and `blaze audit grants`
/// (through [`super::table::grants`] of its value).
pub fn workspace(db: &Db) -> Workspace {
    let crates = base_db::all_crates(db);
    let mut deps: Vec<Building> = Vec::new_in(Global);
    let mut members: Vec<Crate> = Vec::new_in(Global);
    for &krate in crates.iter() {
        let data = krate.data(db);
        if data.origin.is_local() {
            members.push(krate);
            continue;
        }
        let Some(package) = package_of(db, krate) else { continue };
        let at = building(&mut deps, package, krate.extra_data(db).version.as_deref());
        if matches!(data.target_kind, TargetKind::BuildScript) {
            deps[at].offer(Capability::BuildScript, u32::MAX, "has a build script, which runs at build time".into());
            for dep in &data.dependencies {
                let name = crate_name(db, dep.crate_id);
                if FOREIGN_BUILDERS.contains(&name) {
                    // A finding's witness, once per package.
                    let witness = format!("its build script builds or links foreign code with `{name}`");
                    deps[at].offer(Capability::Ffi, u32::MAX, witness.into());
                }
            }
            continue;
        }
        for (capability, witness) in anywhere(db, krate).iter() {
            deps[at].offer(*capability, u32::MAX, witness.clone());
        }
        deps[at].libs.push(krate);
    }
    for &member in &members {
        let reached = reached(db, member);
        for found in reached.iter() {
            let Some(package) = package_of(db, found.krate) else { continue };
            let at = building(&mut deps, package, found.krate.extra_data(db).version.as_deref());
            deps[at].offer(found.capability, found.calls, found.witness.clone());
        }
    }
    let mut out: Vec<Dep> = Vec::with_capacity_in(deps.len(), Global);
    for dep in deps {
        let mut unreached: Vec<Used> = Vec::new_in(Global);
        for &lib in &dep.libs {
            for (capability, witness) in self::unreached(db, lib).iter() {
                let taken = dep.uses.iter().any(|it| it.0 == *capability)
                    || unreached.iter().any(|it| it.capability == *capability)
                    || ANYWHERE.has(*capability);
                if !taken {
                    unreached.push(Used { capability: *capability, witness: witness.clone() });
                }
            }
        }
        unreached.sort_by_key(|it| it.capability);
        let mut uses = dep.uses;
        uses.sort_by_key(|it| it.0);
        let uses: Vec<Used> = {
            let mut out = Vec::with_capacity_in(uses.len(), Global);
            for (capability, _, witness) in uses {
                out.push(Used { capability, witness });
            }
            out
        };
        out.push(Dep {
            name: dep.name,
            version: dep.version,
            uses: uses.into_boxed_slice(),
            unreached: unreached.into_boxed_slice(),
        });
    }
    out.sort_by(|a, b| order(&a.name, &b.name).then_with(|| a.version.cmp(&b.version)));
    Workspace { deps: out.into_boxed_slice() }
}

/// A dependency package as [`workspace`] gathers it.
struct Building {
    name: Box<str>,
    version: Option<Box<str>>,
    /// Each capability once: the calls of its witness from the workspace (`u32::MAX` for one
    /// read off the crate's items) and the witness.
    uses: Vec<(Capability, u32, Box<str>), Global>,
    /// Its crates that are neither a build script nor a proc macro's host: whose functions
    /// [`unreached`] reads.
    libs: Vec<Crate, Global>,
}

impl Building {
    /// `capability` with a witness of `calls` calls: kept when it has fewer than the one kept.
    fn offer(&mut self, capability: Capability, calls: u32, witness: Box<str>) {
        match self.uses.iter_mut().find(|it| it.0 == capability) {
            Some(kept) if calls < kept.1 => *kept = (capability, calls, witness),
            Some(_) => {}
            None => self.uses.push((capability, calls, witness)),
        }
    }
}

/// The index of the package `name` at `version` in `deps`, added when it is not there.
fn building(deps: &mut Vec<Building, Global>, name: &str, version: Option<&str>) -> usize {
    if let Some(at) = deps.iter().position(|it| &*it.name == name && it.version.as_deref() == version) {
        return at;
    }
    deps.push(Building {
        name: name.into(),
        version: version.map(Into::into),
        uses: Vec::new_in(Global),
        libs: Vec::new_in(Global),
    });
    deps.len() - 1
}

/// The package name of a dependency crate: `None` for the workspace's and the toolchain's.
pub fn package_of(db: &Db, krate: Crate) -> Option<&str> {
    if toolchain_crate(db, krate) {
        return None;
    }
    match &krate.data(db).origin {
        CrateOrigin::Library { name, .. } | CrateOrigin::Rustc { name } => Some(name.as_str()),
        CrateOrigin::Local { .. } | CrateOrigin::Lang(_) => None,
    }
}

/// `krate`'s crate name, or nothing for a crate without one.
fn crate_name(db: &Db, krate: Crate) -> &str {
    krate.extra_data(db).display_name.as_ref().map_or("", |it| it.crate_name().as_str())
}

/// One (crate, capability) a workspace crate reaches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reached {
    pub krate: Crate,
    pub capability: Capability,
    /// The calls from the workspace crate's function to the node that uses it.
    pub calls: u32,
    pub witness: Box<str>,
}

/// What the functions of the workspace crate `krate` reach in the dependencies.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn reached(db: &Db, krate: Crate) -> Box<[Reached]> {
    let roots = functions_of(db, krate);
    let reach = reach(db, &roots, None);
    let (graph, crates) = graph_of(db, &reach);
    let mut out: Vec<Reached> = Vec::new_in(Global);
    for found in attribute(&graph) {
        let mut witness = String::new();
        for (at, &node) in found.chain.iter().enumerate() {
            if at > 0 {
                witness.push_str(" → ");
            }
            witness.push_str(&reach.nodes[node as usize].name);
        }
        witness.push(' ');
        witness.push_str(&found.what);
        let calls = u32::try_from(found.chain.len().saturating_sub(1)).unwrap_or(u32::MAX);
        out.push(Reached {
            krate: crates[found.krate as usize],
            capability: found.capability,
            calls,
            witness: witness.into(),
        });
    }
    out.into_boxed_slice()
}

/// What each function of the dependency crate `krate` does itself, reached or not: each
/// capability once, with the first function met that uses it.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn unreached(db: &Db, krate: Crate) -> Box<[(Capability, Box<str>)]> {
    let roots = functions_of(db, krate);
    let reach = reach(db, &roots, Some(krate));
    let mut out: Vec<(Capability, Box<str>)> = Vec::new_in(Global);
    for node in &reach.nodes {
        for done in &node.done {
            for (capability, what) in uses_of(done) {
                if ANYWHERE.has(capability) || out.iter().any(|it| it.0 == capability) {
                    continue;
                }
                out.push((capability, format!("{} {what}", node.name).into()));
            }
        }
    }
    out.sort_by_key(|it| it.0);
    out.into_boxed_slice()
}

/// The graph [`attribute`] reads of `reach`, and the crate of each crate index.
fn graph_of(db: &Db, reach: &Reach) -> (Graph, Vec<Crate, Global>) {
    let mut index: HashMap<Crate, u32, Global> = HashMap::new_in(Global);
    let mut crates: Vec<Crate> = Vec::new_in(Global);
    let mut members: Vec<bool> = Vec::new_in(Global);
    let mut nodes: Vec<Node> = Vec::with_capacity_in(reach.nodes.len(), Global);
    for node in &reach.nodes {
        let krate = *index.entry(node.krate).or_insert_with(|| {
            crates.push(node.krate);
            members.push(node.krate.data(db).origin.is_local());
            u32::try_from(crates.len() - 1).expect("fewer crates than `u32::MAX`")
        });
        let mut uses: Vec<(Capability, Box<str>)> = Vec::new_in(Global);
        for done in &node.done {
            uses.extend(uses_of(done));
        }
        nodes.push(Node { krate, uses: uses.into_boxed_slice(), calls: node.calls.clone() });
    }
    let graph = Graph {
        members: members.into_boxed_slice(),
        roots: reach.roots,
        nodes: nodes.into_boxed_slice(),
    };
    (graph, crates)
}

/// Each capability `done` uses, with what it does, said with what it stands for when the walk
/// could not see it or could not tell its IO apart.
fn uses_of(done: &Done) -> impl Iterator<Item = (Capability, Box<str>)> + '_ {
    let mapped = of_done(done);
    mapped.caps.iter().map(move |capability| {
        let what = if mapped.unseen && UNSEEN_STANDS_FOR.has(capability) {
            format!("{}, which blaze could not see into, so it counts as every capability", done.what)
        } else if mapped.unsplit && IO_SPLIT.has(capability) {
            format!(
                "{}, input or output blaze could not tell apart, so it counts as fs, net and process",
                done.what
            )
        } else {
            done.what.to_string()
        };
        (capability, what.into())
    })
}

/// Every function of `krate` with a body or not, its impls' and traits' included, and those of
/// the modules inside its bodies, in the crate's order.
fn functions_of(db: &Db, krate: Crate) -> Vec<FunctionId, Global> {
    let mut out: Vec<FunctionId> = Vec::new_in(Global);
    for def in bodies(db, krate).0 {
        if let DefWithBodyId::FunctionId(func) = def {
            out.push(func);
        }
    }
    out
}

/// The bodies of `krate` (`check::bodies_of` of each of its modules, and of the modules inside
/// its bodies) and the scopes they are declared in.
fn bodies(db: &Db, krate: Crate) -> (Vec<DefWithBodyId, Global>, Vec<&hir_def::item_scope::ItemScope, Global>) {
    let mut defs: Vec<DefWithBodyId> = Vec::new_in(Global);
    let mut scopes = Vec::new_in(Global);
    for (_, module) in crate_def_map(db, krate).modules(db) {
        check::bodies_of(db, module.scope(db), &mut defs);
        scopes.push(module.scope(db));
    }
    let mut next = 0;
    while let Some(&def) = defs.get(next) {
        next += 1;
        if has_body(db, def) {
            for (_, block) in Body::of(db, def).blocks(db) {
                for (_, module) in block.modules(db) {
                    check::bodies_of(db, module.scope(db), &mut defs);
                    scopes.push(module.scope(db));
                }
            }
        }
    }
    (defs, scopes)
}

/// Whether `def` has a body to read: not a foreign item, not a function or static declared
/// without one.
fn has_body(db: &Db, def: DefWithBodyId) -> bool {
    match def {
        DefWithBodyId::FunctionId(func) => {
            !foreign(db, func) && FunctionSignature::of(db, func).has_body()
        }
        DefWithBodyId::StaticId(id) => !StaticSignature::of(db, id).flags.contains(StaticFlags::EXTERN),
        DefWithBodyId::ConstId(_) | DefWithBodyId::VariantId(_) => true,
    }
}

fn foreign(db: &Db, func: FunctionId) -> bool {
    matches!(func.loc(db).container, ItemContainerId::ExternBlockId(_))
}

/// What the items of the dependency crate `krate` say wherever they are: each capability once,
/// with the first item met that says it.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn anywhere(db: &Db, krate: Crate) -> Box<[(Capability, Box<str>)]> {
    let mut out: Vec<(Capability, Box<str>)> = Vec::new_in(Global);
    let mut say = |capability: Capability, witness: String| {
        if !out.iter().any(|it| it.0 == capability) {
            out.push((capability, witness.into()));
        }
    };
    if krate.data(db).target_kind.is_proc_macro() {
        say(Capability::ProcMacro, "is a proc-macro crate, whose code runs inside the compiler".to_owned());
    }
    let (defs, scopes) = bodies(db, krate);
    for &def in &defs {
        match def {
            DefWithBodyId::FunctionId(func) => {
                let path = function_path(db, func);
                if foreign(db, func) {
                    say(Capability::Ffi, format!("declares the foreign function `{path}`"));
                    say(Capability::Unsafe, format!("declares the foreign function `{path}`"));
                    continue;
                }
                if FunctionSignature::of(db, func).is_unsafe() {
                    say(Capability::Unsafe, format!("`{path}` is an `unsafe fn`"));
                }
                symbol_attrs(db, AttrDefId::FunctionId(func), &path, &mut say);
            }
            DefWithBodyId::StaticId(id) => {
                let signature = StaticSignature::of(db, id);
                let name = signature.name.as_str();
                if signature.flags.contains(StaticFlags::EXTERN) {
                    say(Capability::Ffi, format!("declares the foreign static `{name}`"));
                    say(Capability::Unsafe, format!("declares the foreign static `{name}`"));
                    continue;
                }
                symbol_attrs(db, AttrDefId::StaticId(id), name, &mut say);
            }
            DefWithBodyId::ConstId(_) | DefWithBodyId::VariantId(_) => {}
        }
        if !has_body(db, def) {
            continue;
        }
        let body = Body::of(db, def);
        for (_, expr) in body.exprs() {
            match expr {
                Expr::Block { unsafe_: Unsafe::Yes, .. } => {
                    say(Capability::Unsafe, format!("{} has an `unsafe` block", owner_name(db, def)));
                }
                Expr::InlineAsm(_) => {
                    say(Capability::Unsafe, format!("{} has inline assembly", owner_name(db, def)));
                }
                _ => {}
            }
        }
    }
    for scope in &scopes {
        for imp in scope.impls() {
            if ImplSignature::of(db, imp).flags.contains(ImplFlags::UNSAFE) {
                say(Capability::Unsafe, "has an `unsafe impl`".to_owned());
            }
        }
        for def in scope.declarations() {
            if let ModuleDefId::TraitId(trait_) = def {
                let signature = TraitSignature::of(db, trait_);
                if signature.flags.contains(TraitFlags::UNSAFE) {
                    let name = signature.name.as_str();
                    say(Capability::Unsafe, format!("`{name}` is an `unsafe trait`"));
                }
            }
        }
    }
    out.sort_by_key(|it| it.0);
    out.into_boxed_slice()
}

/// The attributes that name a symbol or a section, which only `unsafe` may write (Rust 2024's
/// `#[unsafe(no_mangle)]`): each is [`Capability::Unsafe`].
fn symbol_attrs(db: &Db, owner: AttrDefId, name: &str, say: &mut impl FnMut(Capability, String)) {
    let flags = AttrFlags::query(db, owner);
    for (flag, attr) in [
        (AttrFlags::NO_MANGLE, "#[no_mangle]"),
        (AttrFlags::HAS_EXPORT_NAME, "#[export_name]"),
        (AttrFlags::HAS_LINK_SECTION, "#[link_section]"),
    ] {
        if flags.contains(flag) {
            say(Capability::Unsafe, format!("`{name}` is `{attr}`"));
        }
    }
}

/// The item a body is, as a witness names it.
fn owner_name(db: &Db, def: DefWithBodyId) -> String {
    match def {
        DefWithBodyId::FunctionId(func) => format!("`{}`", function_path(db, func)),
        DefWithBodyId::StaticId(id) => format!("`{}`", StaticSignature::of(db, id).name.as_str()),
        DefWithBodyId::ConstId(_) => "a constant".to_owned(),
        DefWithBodyId::VariantId(_) => "an enum's discriminant".to_owned(),
    }
}

/// The capabilities of `uses`.
pub fn caps_of(uses: &[Used]) -> Caps {
    uses.iter().fold(Caps::EMPTY, |caps, it| caps.union(Caps::one(it.capability)))
}
