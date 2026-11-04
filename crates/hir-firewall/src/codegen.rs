//! What codegen compiles of a crate: every instance its functions reach, each by what its
//! store key is made of (its code hash and the facts of its type arguments).

use std::alloc::Global;
use std::{collections::HashMap, sync::Arc};

use base_db::{Crate, Db, FileId};
use blaze_codegen::{
    Owner,
    backend::{FrontRef, Frontend},
    plan::{Mode, OptLevel, Origin, Override, Package, Policy, Profile},
    settings::{DebugInfo, Panic, Pgo, Sanitize},
};
use blaze_ids::ContentId;
use hir_ty_codegen::v2::{
    BuildTime, Databases, HashSource, HirFrontend, HirInstance, ItemHashes, Runs,
};

/// How the harness compiles: a dev build's policy (opt-level 0, where no function's code
/// builds in another workspace body), the origin the crate's code is asked as, and the names
/// its files are known by.
pub struct Codegen {
    pub policy: Policy,
    pub origin: Origin,
    /// A file's path as the build names it; a file it does not list is named by its id.
    pub names: Arc<HashMap<FileId, String>>,
}

impl Codegen {
    /// A dev build of workspace member `package` for this machine, whose crates are laid out by
    /// `target_data` (the host's).
    pub fn dev(
        package: &str,
        names: Arc<HashMap<FileId, String>>,
        target_data: &base_db::target::TargetData,
    ) -> Result<Codegen, String> {
        // The harness's own code generator: it keeps no store, so nothing it compiles is read by
        // another blaze build.
        let code = hir_ty_codegen::v2::CodegenCode(blaze_ids::ContentId::of(
            "blaze v2 hir-firewall code",
            b"",
        ));
        let toolchain = hir_ty_codegen::v2::toolchain_for(
            &code,
            hir_ty_codegen::Target::host(),
            target_data,
            &[],
            Runs::Here,
        )?;
        hir_ty_codegen::v2::select(&toolchain)?;
        let policy = Policy {
            toolchain,
            profile: Profile {
                opt_level: OptLevel::O0,
                overflow_checks: true,
                debug_assertions: true,
                debuginfo: DebugInfo::None,
                panic: Panic::Unwind,
                packages: Default::default(),
                build_override: Override::default(),
                stencils: false,
            },
            mode: Mode::Output,
            fold: false,
            sanitize: Sanitize::default(),
            pgo: Pgo::None,
            proc_macro_bridge: false,
            report: false,
            fuel: false,
            heap_to_stack: false,
        };
        Ok(Codegen { policy, origin: Origin::Member(Package::new(package)), names })
    }
}

/// What an instance's store key is made of: its code hash and its arguments' facts.
pub(crate) type Keys = blaze_hash::HashMap<HirInstance, (ContentId, ContentId), Global>;

/// Every instance `krate`'s functions reach, by what its key is made of. A dependency's
/// instance is listed, and what it reaches is not walked: no edit of the crate reaches into
/// it ([`Owner::Dependency`]).
pub(crate) fn instances(db: &Db, krate: Crate, codegen: &Codegen) -> Result<Keys, String> {
    let names = Arc::clone(&codegen.names);
    let hashes = ItemHashes::new(Box::new(move |file: FileId| {
        // alloc: tests only, once a pass
        Some(names.get(&file).cloned().unwrap_or_else(|| format!("file{}.rs", file.index()))) // alloc: clone: tests only, a file's name
    }));
    let build_time = BuildTime::default();
    // Split as a dev executable that links the dependency image splits it, whose owners
    // follow what an edit reaches: without the image every crate's code is the workspace's.
    let frontend = HirFrontend::new(Databases::new(db), HashSource::Items(hashes), build_time)
        .with_policy(&codegen.policy)
        .with_image(true);
    let mut queue = frontend.every_function(krate);
    let mut keys = Keys::default();
    while let Some(instance) = queue.pop() {
        if keys.contains_key(&instance) {
            continue;
        }
        let described =
            frontend.describe(&instance, &codegen.origin).map_err(|error| error.what)?;
        if described.owner == Owner::Workspace {
            for reference in &described.refs {
                if let FrontRef::Function(callee) = reference
                    && !keys.contains_key(callee)
                {
                    // clone: tests only, a handle of the instance
                    queue.push(callee.clone());
                }
            }
        }
        keys.insert(instance, (described.code.0, described.args));
    }
    Ok(keys)
}
