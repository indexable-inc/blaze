//! The round loop of a crate's collection and the assembly of its `DefMap`
//! (`docs/v2/plan/lanes/frontend/nameres-core.md` §5 `driver.rs`), and a block's collection on
//! one unit.
//!
//! A crate's rounds are queries: `round_modules` (the round's modules in order), `module_round`
//! (what each gains from the steps before), `import_shape` (the round's import graph),
//! `round_env` (the crate-level state), `round_progress` and `round_kind` (the old
//! `resolution_loop`'s decisions). `collect_crate` walks the rounds until `RoundKind::Final` and
//! builds the def map from the final round's `fact_handle`s: no scope is copied.

use base_db::{Crate, Db};
use blaze_hash::HashMap;
use blaze_mem::Frame;
use hir_expand::{
    MacroCallId, MacroDefId,
    mod_path::{ModPath, PathKind},
    name::Name,
};
use stdx::always;
use syntax::ast;
use triomphe::Arc;

use crate::{
    AstId, ExternCrateId, Lookup, ModuleDefId, ModuleId,
    item_scope::{GlobId, ImportId, ImportOrExternCrate},
    item_tree::{ImportKind, TreeId},
    nameres::{
        BuiltinShadowMode, DefMap, DefMapCrateData, DeriveHelpers, DiagnosticsByModule,
        LocalDefMap, MacroSubNs, ModuleData, ModulesMap, ModulesSlot, ResolveMode, ScopeSlot,
        diagnostics::{DefDiagnostic, DefDiagnosticKind, is_glob_of_non_module},
        uses::{DeriveHelperUse, MacroCallRecord, MacroVia, NameRecords, Use},
    },
    per_ns::{Item, Namespace, PerNs},
    visibility::RawVisibility,
};

use super::{
    FrameMap, FrameSet, FrameVec, collect,
    env::{ChildMode, ChildRequest, CrateEnv, SideTables, SideTablesIn, Sink},
    graph::{ImportShape, ShapeImport, ShapeModule},
    macros, map_in,
    ops::{DeclOp, DeriveHelper, ImportOp, ImportSource, MacroOp, MacroOpKind, OpLog},
    queries::{
        self, CrateEnvKey, FactKey, ModuleRound, ModuleRow, NO_ROUND, RoundKey, RoundKind,
        RoundModules, RoundOutcome, SccKey, SccState, Scope,
    },
    set_in,
    unit::Unit,
};

/// The old loop's bound on expansion rounds (`collector::FIXED_POINT_LIMIT`).
pub(crate) const FIXED_POINT_LIMIT: u32 = 8192;

/// The error of a resolution of `krate` (a block's when `block`) that passed
/// [`FIXED_POINT_LIMIT`] after `rounds`, naming what was still pending: `macros`, and the
/// `imports` that had not resolved.
pub(crate) fn fixpoint_limit<'a>(
    db: &Db,
    krate: Crate,
    block: bool,
    rounds: u32,
    macros: impl Iterator<Item = &'a MacroOp>,
    imports: impl Iterator<Item = &'a ImportOp>,
) -> crate::nameres::FixpointLimit {
    use crate::nameres::FixpointLimit;
    let edition = span::Edition::LATEST;
    let module = |it: ModuleId| match it.name(db) {
        Some(name) => format!("module `{}`", name.display(db, edition)), // alloc: an error's text, once
        None => "the crate root".to_owned(),                             // alloc: as above
    };
    let mut first = Vec::new(); // alloc: an error's list, once
    let mut pending_macros = 0;
    for op in macros {
        pending_macros += 1;
        if first.len() < FixpointLimit::LISTED {
            let path = match &op.kind {
                MacroOpKind::FnLike { ast_id, .. } => &ast_id.path,
                MacroOpKind::Derive { ast_id, .. } => &ast_id.path,
                MacroOpKind::Attr { attr, .. } => &attr.path,
            };
            first.push(format!("macro `{}` in {}", path.display(db, edition), module(op.module))); // alloc: as above
        }
    }
    let mut pending_imports = 0;
    for op in imports {
        pending_imports += 1;
        if first.len() < 2 * FixpointLimit::LISTED {
            first.push(format!("import `{}` in {}", op.path.display(db, edition), module(op.module))); // alloc: as above
        }
    }
    FixpointLimit {
        krate,
        // alloc: an error's crate name, once
        name: krate.extra_data(db).display_name.as_ref().map(|it| it.crate_name().to_string()),
        block,
        rounds,
        pending_macros,
        pending_imports,
        first,
    }
}

// The round queries.

/// Every module of the round in the round's module order: round 0's DFS preorder of
/// `module_decls` from the root (the old map's insertion order), then each later round's new
/// modules in the order their steps declared them, steps in module order. With them, the
/// round's active set (`nameres-core.md` §9): read off the steps of the round before that ran,
/// so a module no step touched costs a copy of its columns and no query.
pub(crate) fn round_modules(db: &Db, frame: &Frame, round: RoundKey) -> RoundModules {
    let krate = round.krate(db);
    let r = round.round(db);
    if r == 0 {
        let mut order = Vec::new_in(frame);
        let mut rows = Vec::new_in(frame);
        let mut flags = Vec::new_in(frame);
        let mut declared = set_in(frame);
        let mut stack = Vec::new_in(frame);
        stack.push(queries::root_decl(db, krate));
        while let Some(key) = stack.pop() {
            let decl = key.decl(db);
            assert!(declared.insert(decl.module), "nameres facts: {:?} is declared twice at round 0", decl.module);
            order.push(decl.module);
            rows.push(ModuleRow {
                origin: decl.origin,
                visibility: decl.visibility,
                parent: decl.parent,
                since: 0,
                decl: Some(key),
            });
            let decls = queries::module_decls(db, key);
            // Round 0 is `Normal`: a module with a directive steps.
            flags.push(match has_directive(&decls.ops, decl.module) {
                true => RoundModules::GAINS | RoundModules::PENDING | RoundModules::STEP,
                false => RoundModules::GAINS,
            });
            stack.extend(decls.children.iter().rev().copied());
        }
        let n = order.len();
        let pending = flags.iter().filter(|&&it| it & RoundModules::PENDING != 0).count() as u32;
        return RoundModules {
            // alloc: the round's columns, at exact size, once per crate.
            order: Box::from(&order[..]),
            // alloc: as `order`.
            rows: rows.into_iter().collect(),
            index: module_index(&order),
            // alloc: as `order`.
            flags: Box::from(&flags[..]),
            // alloc: as `order`.
            active: (0..n as u32).collect(),
            // alloc: as `active`.
            last_step: vec![NO_ROUND; n].into_boxed_slice(),
            // alloc: as `active`.
            ran: vec![NO_ROUND; n].into_boxed_slice(),
            // alloc: as `active`.
            since: vec![NO_ROUND; n].into_boxed_slice(),
            pending,
        };
    }

    let before = RoundKey::new(db, krate, r - 1);
    let previous = queries::round_modules(db, before);
    let root = queries::crate_root(db, krate);
    // A round's columns are the round before's plus its new modules: `O(m)` per round.
    let mut order = Vec::with_capacity_in(previous.order.len(), frame);
    order.extend_from_slice(&previous.order);
    let mut rows = Vec::with_capacity_in(previous.rows.len(), frame);
    // clone: as `order`.
    rows.extend(previous.rows.iter().cloned());
    let mut declared = set_in(frame);
    // A module that did not step keeps its directives; one that did has what its step left.
    let mut flags = Vec::with_capacity_in(previous.flags.len(), frame);
    flags.extend(previous.flags.iter().map(|&it| it & RoundModules::PENDING));
    let mut last_step = Vec::with_capacity_in(previous.last_step.len(), frame);
    last_step.extend_from_slice(&previous.last_step);
    for at in previous.stepping() {
        let module = previous.order[at as usize];
        let step = queries::macro_step(db, FactKey::new(db, module, krate, r - 1));
        last_step[at as usize] = r - 1;
        let mut own = match step.pending.is_empty() {
            true => 0,
            false => RoundModules::PENDING,
        };
        for op in step.ops.iter() {
            match op.target() {
                Some(target) if target == module && op.is_resolution_op() => own |= RoundModules::GAINS,
                Some(target) if target == root && module != root => flags[0] |= RoundModules::GAINS,
                _ => {}
            }
        }
        flags[at as usize] = (flags[at as usize] & RoundModules::GAINS) | own;
        for (new, row) in step.new_modules.iter() {
            debug_assert_eq!(row.since, r, "a module the step of round {} declared", r - 1);
            let fresh = !previous.index.contains_key(new) && declared.insert(*new);
            assert!(fresh, "nameres facts: {new:?} is declared twice, again by {module:?} at round {}", r - 1);
            order.push(*new);
            // clone: a new module's row, once.
            rows.push(row.clone());
            flags.push(match has_directive(&step.ops, *new) {
                true => RoundModules::GAINS | RoundModules::PENDING,
                false => RoundModules::GAINS,
            });
            last_step.push(NO_ROUND);
        }
    }

    let index = module_index(&order);
    let kind = queries::round_kind(db, round);
    match kind {
        RoundKind::Normal | RoundKind::EagerWithErrors => {
            for it in flags.iter_mut().filter(|it| **it & RoundModules::PENDING != 0) {
                *it |= RoundModules::STEP;
            }
        }
        RoundKind::Reseed { module, .. } => {
            let at = index_of_in(&index, module, r);
            flags[at as usize] |= RoundModules::STEP;
        }
        RoundKind::Final => {}
    }

    // Where each module of the round before stands after it: the round its component last ran
    // at, and the round its facts last changed at.
    let n = previous.order.len();
    let mut ran = Vec::with_capacity_in(order.len(), frame);
    ran.extend_from_slice(&previous.ran);
    let mut since = Vec::with_capacity_in(order.len(), frame);
    since.extend_from_slice(&previous.since);
    let active_before = (r > 1).then(|| (queries::round_active(db, before), queries::partition(db, before)));
    for at in 0..n {
        let module = previous.order[at];
        let ran_before = match active_before {
            Some((active, partition)) => active.runs(partition.of(at as u32)),
            None => true,
        };
        if !ran_before {
            continue;
        }
        ran[at] = r - 1;
        since[at] = match previous.since(at as u32) {
            None => r - 1,
            Some(t) => {
                let now = queries::fact_handle(db, FactKey::new(db, module, krate, r - 1));
                match now == queries::fact_handle(db, FactKey::new(db, module, krate, t)) {
                    true => t,
                    false => r - 1,
                }
            }
        };
    }
    ran.resize(order.len(), NO_ROUND);
    since.resize(order.len(), NO_ROUND);

    let touched = RoundModules::STEP | RoundModules::GAINS;
    let active = || (0u32..).zip(flags.iter()).filter(|&(_, &it)| it & touched != 0).map(|(at, _)| at);
    // alloc: the round's active modules, a few, at exact size.
    let mut active_list = Vec::with_capacity(active().count());
    active_list.extend(active());
    let pending = flags.iter().filter(|&&it| it & RoundModules::PENDING != 0).count() as u32;
    RoundModules {
        // alloc: the round's columns, at exact size: `O(m)` per round.
        order: Box::from(&order[..]),
        // alloc: as `order`.
        rows: rows.into_iter().collect(),
        index,
        // alloc: as `order`.
        flags: Box::from(&flags[..]),
        active: active_list.into_boxed_slice(),
        // alloc: as `order`.
        last_step: Box::from(&last_step[..]),
        // alloc: as `order`.
        ran: Box::from(&ran[..]),
        // alloc: as `order`.
        since: Box::from(&since[..]),
        pending,
    }
}

/// The round's index of each module in `order`, sized once.
fn module_index(order: &[ModuleId]) -> HashMap<ModuleId, u32> {
    // alloc: the round's index, at its size, once per round.
    let mut index = HashMap::with_capacity_and_hasher(order.len(), Default::default());
    index.extend((0u32..).zip(order).map(|(at, &module)| (module, at)));
    index
}

/// Whether `log` holds a macro directive of `module`: what its first step starts from.
fn has_directive(log: &OpLog, module: ModuleId) -> bool {
    log.iter().any(|op| matches!(op, DeclOp::Macro(it) if it.module == module))
}

fn index_of_in(index: &HashMap<ModuleId, u32>, module: ModuleId, round: u32) -> u32 {
    *index.get(&module).unwrap_or_else(|| panic!("nameres facts: {module:?} is not a module of round {round}"))
}

/// What module `key.module` gains at round `key.round >= 1` (the contract's §3 rule): the
/// resolution ops its own step of the round before made in it or, for a module that step
/// declared, the ops of the declaring step with it as their target; then, for the crate root,
/// the root ops of every other step of that round, steps in module order. Only the steps that
/// ran are read: a module that did not step at the round before made no op.
pub(crate) fn module_round(db: &Db, frame: &Frame, key: FactKey) -> ModuleRound {
    let (module, krate, r) = (key.module(db), key.krate(db), key.round(db));
    assert!(r >= 1, "nameres facts: module_round of {module:?} at round 0, whose ops are module_decls'");
    let modules = queries::round_modules(db, RoundKey::new(db, krate, r));
    let row = modules
        .row(module)
        .unwrap_or_else(|| panic!("nameres facts: {module:?} is not a module of round {r}"));
    let previous = queries::round_modules(db, RoundKey::new(db, krate, r - 1));
    let mut ops = Vec::new_in(frame);
    if row.since == r {
        let declarer = declaring_module(modules, module, row, r);
        let step = queries::macro_step(db, FactKey::new(db, declarer, krate, r - 1));
        // clone: every op a module gains is copied once, from the step (a query value read in
        // place) to the module's round.
        ops.extend(step.ops.iter().filter(|op| op.target() == Some(module)).filter_map(carried_op));
    } else if previous.at(module).is_some_and(|at| previous.steps(at)) {
        let step = queries::macro_step(db, FactKey::new(db, module, krate, r - 1));
        // clone: as above.
        ops.extend(
            step.ops
                .iter()
                .filter(|op| op.target() == Some(module))
                .filter_map(DeclOp::clone_resolution_op),
        );
    }

    let root = queries::crate_root(db, krate);
    if module == root {
        for at in previous.stepping() {
            let other = previous.order[at as usize];
            if other == root {
                continue;
            }
            let step = queries::macro_step(db, FactKey::new(db, other, krate, r - 1));
            for op in step.ops.iter().filter(|op| op.target() == Some(root)) {
                // clone: a `#[macro_export]` (or proc-macro) op of another module, once.
                let op = op.clone_root_op(root).unwrap_or_else(|| {
                    panic!(
                        "nameres facts: the step of {other:?} at round {} made {op:?} in the crate root, which only its own step may",
                        r - 1
                    )
                });
                ops.push(op);
            }
        }
    }
    ModuleRound { ops: OpLog::from_frame(ops) }
}

/// The module whose step of round `round - 1` declared `module` (declared at `round`): its
/// nearest ancestor that existed before, as an expansion's modules nest in the module it was
/// collected into.
fn declaring_module(modules: &RoundModules, module: ModuleId, row: &ModuleRow, round: u32) -> ModuleId {
    let mut at = row;
    loop {
        let parent = at.parent.unwrap_or_else(|| {
            panic!("nameres facts: {module:?}, declared at round {round}, has no ancestor of an earlier round")
        });
        let parent_row = modules.row(parent).unwrap_or_else(|| {
            panic!("nameres facts: the parent {parent:?} of a module of round {round} is not in the round")
        });
        if parent_row.since < round {
            return parent;
        }
        at = parent_row;
    }
}

/// A copy of an op a declaring step made in a module it declared: the resolution ops, and the
/// module's macro directives and mod dir, which its first step starts from.
fn carried_op(op: &DeclOp) -> Option<DeclOp> {
    match op {
        // clone: a directive of a new module, moved to its round once.
        DeclOp::Macro(it) => Some(DeclOp::Macro(it.clone())),
        &DeclOp::SetModDir { module, dir } => Some(DeclOp::SetModDir { module, dir }),
        op => op.clone_resolution_op(),
    }
}

/// The round's import graph input, per module of the round in order (`graph.rs` reads it): the
/// parent, the names that denote a local module by declaration, every import of rounds
/// `0..=r`, the `pub(in path)` visibilities of those imports. Built from the round before's
/// shape plus what each module gains at this round.
pub(crate) fn import_shape(db: &Db, frame: &Frame, round: RoundKey) -> ImportShape {
    let (krate, r) = (round.krate(db), round.round(db));
    let modules = queries::round_modules(db, round);
    let root = queries::crate_root(db, krate);
    let edition = queries::crate_data(db, krate).edition;
    let extern_prelude = &queries::round_env(db, round).env(db).extern_prelude;
    let names = || extern_prelude.iter().filter(|&&(_, module, _)| module == root).map(|&(name, ..)| name);
    // alloc: the shape's value, a few names, at exact size.
    let mut extern_prelude_self = Vec::with_capacity(names().count());
    extern_prelude_self.extend(names());
    let previous = match r {
        0 => None,
        _ => Some(queries::import_shape(db, RoundKey::new(db, krate, r - 1))),
    };
    debug_assert!(previous.is_none_or(|it| it.modules.len() <= modules.order.len()));

    // alloc: the shape's value, filled to its capacity: one per module of the round.
    let mut out = Vec::with_capacity(modules.order.len());
    let mut scratch = ShapeScratch::new_in(frame);
    for (at, (&module, row)) in modules.order.iter().zip(modules.rows.iter()).enumerate() {
        let before = previous.and_then(|it| it.modules.get(at));
        if let Some(before) = before
            && !modules.gains(at as u32)
        {
            // clone: a module that gains no op this round keeps its shape.
            out.push(before.clone());
            continue;
        }
        let ops = match r {
            0 => {
                let key = row.decl.unwrap_or_else(|| {
                    panic!("nameres facts: {module:?} of round 0 has no round-0 key")
                });
                &queries::module_decls(db, key).ops
            }
            _ => &queries::module_round(db, FactKey::new(db, module, krate, r)).ops,
        };
        if let Some(before) = before
            && !ops.iter().any(|op| shapes(op, module, root))
        {
            // clone: a module that gains no import or module name this round keeps its shape.
            out.push(before.clone());
            continue;
        }
        scratch.clear();
        if let Some(before) = before {
            scratch.extend_from(before);
        }
        for op in ops.iter() {
            scratch.add(op, module, root, modules);
        }
        out.push(ShapeModule {
            parent: row.parent.map(|parent| index_of(modules, parent)),
            local_names: scratch.local_names.as_slice().into(),
            imports: scratch.imports.as_slice().into(),
            other_paths: scratch.other_paths.as_slice().into(),
        });
    }
    ImportShape {
        krate,
        edition,
        modules: out.into_boxed_slice(),
        extern_prelude_self: extern_prelude_self.into_boxed_slice(),
    }
}

/// The index of a module of the round in its order.
fn index_of(modules: &RoundModules, module: ModuleId) -> u32 {
    *modules
        .index
        .get(&module)
        .unwrap_or_else(|| panic!("nameres facts: {module:?} is not a module of the round"))
}

/// Whether `op` of `module`'s log changes its import shape.
fn shapes(op: &DeclOp, module: ModuleId, root: ModuleId) -> bool {
    match op {
        DeclOp::Import(it) => it.module == module,
        DeclOp::PushChild { parent, .. } => *parent == module,
        DeclOp::Update { module: target, import: Some(ImportOrExternCrate::ExternCrate(_)), .. } => {
            *target == module
        }
        DeclOp::ExternPrelude { root: it, .. } => *it == root && module == root,
        _ => false,
    }
}

/// One module's shape while it is built, reused from module to module.
struct ShapeScratch<'f> {
    local_names: FrameVec<'f, (Name, u32)>,
    imports: FrameVec<'f, ShapeImport>,
    other_paths: FrameVec<'f, ModPath>,
}

impl<'f> ShapeScratch<'f> {
    fn new_in(frame: &'f Frame) -> Self {
        ShapeScratch { local_names: Vec::new_in(frame), imports: Vec::new_in(frame), other_paths: Vec::new_in(frame) }
    }

    fn clear(&mut self) {
        self.local_names.clear();
        self.imports.clear();
        self.other_paths.clear();
    }

    fn extend_from(&mut self, before: &ShapeModule) {
        self.local_names.extend_from_slice(&before.local_names);
        self.imports.extend_from_slice(&before.imports);
        self.other_paths.extend_from_slice(&before.other_paths);
    }

    fn add(&mut self, op: &DeclOp, module: ModuleId, root: ModuleId, modules: &RoundModules) {
        match op {
            DeclOp::Import(import) if import.module == module => {
                let is_glob = matches!(import.source.kind, ImportKind::Glob);
                self.imports.push(ShapeImport {
                    path: import.path,
                    binds: if is_glob { None } else { import.bound_name() },
                    is_glob,
                    is_prelude: import.source.is_prelude,
                });
                // The use trees of one `use` item share its visibility: kept once.
                if let RawVisibility::Module(path, _) = &import.visibility
                    && !path.segments().is_empty()
                    && self.other_paths.last() != Some(path)
                {
                    self.other_paths.push(*path);
                }
            }
            &DeclOp::PushChild { parent, name, child } if parent == module => {
                self.local_names.push((name, index_of(modules, child)));
            }
            // `extern crate self as n` binds `n` to the crate root.
            DeclOp::Update {
                module: target,
                resolutions,
                import: Some(ImportOrExternCrate::ExternCrate(_)),
                ..
            } if *target == module => {
                for (name, def) in resolutions.iter() {
                    if let (Some(name), Some(Item { def: ModuleDefId::ModuleId(it), .. })) =
                        (name, &def.types)
                        && let Some(&at) = modules.index.get(it)
                    {
                        self.local_names.push((*name, at));
                    }
                }
            }
            &DeclOp::ExternPrelude { name, root: it, .. } if it == root && module == root => {
                self.local_names.push((name, index_of(modules, root)));
            }
            _ => {}
        }
    }
}

/// The crate env the round's units and steps start from: `root_decl`'s env folded with the
/// root's round-0 log's crate-level ops (round 0), or the round before's folded with the root's
/// step of that round, if the root stepped.
pub(crate) fn round_env(db: &Db, round: RoundKey) -> CrateEnvKey {
    let (krate, r) = (round.krate(db), round.round(db));
    if r == 0 {
        let root = queries::root_decl(db, krate);
        let start = root.decl(db).env.env(db).krate;
        return after_all(db, start, &queries::module_decls(db, root).ops);
    }
    let before = RoundKey::new(db, krate, r - 1);
    let start = queries::round_env(db, before);
    // The crate root is the first module of every round.
    if !queries::round_modules(db, before).steps(0) {
        return start;
    }
    let root = queries::crate_root(db, krate);
    after_all(db, start, &queries::macro_step(db, FactKey::new(db, root, krate, r - 1)).ops)
}

/// `start`'s env after the crate-level ops of `ops`; `start` itself, interned once, when none
/// changes it.
fn after_all(db: &Db, start: CrateEnvKey, ops: &OpLog) -> CrateEnvKey {
    let mut changed: Option<CrateEnv> = None;
    for op in ops.iter() {
        let next = changed.as_ref().unwrap_or_else(|| start.env(db)).after(op);
        if next.is_some() {
            changed = next;
        }
    }
    match changed {
        Some(env) => CrateEnvKey::new(db, env),
        None => start,
    }
}

/// The fold of the round's macro steps over its modules in order: whether any made progress or
/// left an eager call for the error round, and the first attribute left, by module order then
/// position. A reseed round always makes progress. Only the steps that ran are read; a module
/// that did not step left its directives as they were.
pub(crate) fn round_progress(db: &Db, round: RoundKey) -> RoundOutcome {
    let (krate, r) = (round.krate(db), round.round(db));
    let kind = queries::round_kind(db, round);
    let mut out = RoundOutcome {
        progress: matches!(kind, RoundKind::Reseed { .. }),
        deferred_eager: false,
        first_attr: None,
    };
    let modules = queries::round_modules(db, round);
    for at in modules.stepping() {
        let step = queries::macro_step(db, FactKey::new(db, modules.order[at as usize], krate, r));
        out.progress |= step.progress;
        out.deferred_eager |= step.deferred_eager;
    }
    let with_directives = (0u32..).zip(modules.flags.iter()).filter(|&(_, &it)| {
        it & (RoundModules::STEP | RoundModules::PENDING) != 0
    });
    for (at, _) in with_directives {
        let module = modules.order[at as usize];
        let index = match modules.steps(at) {
            true => macros::first_attr(queries::macro_step(db, FactKey::new(db, module, krate, r)).pending.iter()),
            false => macros::first_attr(macros::Carried::at(db, krate, modules, at).directives()),
        };
        if let Some(index) = index {
            out.first_attr = Some((module, index));
            break;
        }
    }
    out
}

/// What the round does after its import fixed point: the old `resolution_loop`'s choice after
/// the round before.
pub(crate) fn round_kind(db: &Db, round: RoundKey) -> RoundKind {
    let (krate, r) = (round.krate(db), round.round(db));
    if r == 0 {
        return RoundKind::Normal;
    }
    let previous = RoundKey::new(db, krate, r - 1);
    let kind = queries::round_kind(db, previous);
    if kind == RoundKind::Final {
        return RoundKind::Final;
    }
    let outcome = queries::round_progress(db, previous);
    if outcome.progress {
        if r > FIXED_POINT_LIMIT {
            let modules = queries::round_modules(db, previous);
            let pending = modules.stepping().flat_map(|at| {
                let key = FactKey::new(db, modules.order[at as usize], krate, r - 1);
                queries::macro_step(db, key).pending.iter()
            });
            fixpoint_limit(db, krate, false, r, pending, std::iter::empty()).throw();
        }
        return RoundKind::Normal;
    }
    if kind == RoundKind::Normal && outcome.deferred_eager {
        return RoundKind::EagerWithErrors;
    }
    match outcome.first_attr {
        Some((module, index)) => RoundKind::Reseed { module, index },
        None => RoundKind::Final,
    }
}

/// The row of `module` in the views of the round: its declaration, and its scope as the
/// round's fact, named by the round it last changed at if its component does not run.
pub(crate) fn round_row(db: &Db, round: RoundKey, module: ModuleId) -> Option<ModuleData> {
    let modules = queries::round_modules(db, round);
    let at = modules.at(module)?;
    let row = &modules.rows[at as usize];
    let fact = queries::fact_key(db, round, at);
    Some(ModuleData {
        origin: row.origin,
        visibility: row.visibility,
        parent: row.parent,
        scope: queries::fact_slot(db, Scope::Fact(fact)),
    })
}

// A crate.

/// The crate's def map: the rounds until `RoundKind::Final`, then one `Modules` of the final
/// round's handles, the crate-level ops of every log, the final steps' unresolved-macro
/// diagnostics and the final units' unresolved imports (`collect` and `finish` of the old
/// collector).
#[cfg(test)]
pub(crate) fn collect_crate<'db>(db: &'db Db, frame: &Frame, krate: Crate) -> (DefMap, LocalDefMap) {
    let mut r = 0;
    while queries::round_kind(db, RoundKey::new(db, krate, r)) != RoundKind::Final {
        blaze_query::db::unwind_if_cancelled(db);
        r += 1;
    }
    let round = RoundKey::new(db, krate, r);
    let root = queries::crate_root(db, krate);
    let modules = queries::round_modules(db, round);
    assert_eq!(modules.order.first(), Some(&root), "nameres facts: the round's first module is not the crate root");

    let mut table = ModulesMap::new();
    table.reserve(modules.order.len());
    for (&module, row) in modules.order.iter().zip(modules.rows.iter()) {
        let handle = queries::fact_handle(db, FactKey::new(db, module, krate, r));
        table.insert(
            module,
            ModuleData {
                origin: row.origin,
                visibility: row.visibility,
                parent: row.parent,
                scope: ScopeSlot::Collected(handle),
            },
        );
    }

    let mut crate_ops = CrateOps {
        diagnostics: Vec::new_in(frame),
        // clone: one count; the data is copied only if a log exports a derive or a proc macro.
        data: queries::crate_data(db, krate).clone(),
        data_changed: false,
        session_macros: false,
        unresolved_extern_crates: set_in(frame),
        records: Records::new_in(frame),
    };
    for row in queries::round_modules(db, RoundKey::new(db, krate, 0)).rows.iter() {
        let key = row.decl.expect("a module of round 0 has its round-0 key");
        crate_ops.take(&queries::module_decls(db, key).ops);
    }
    for at in 0..r {
        let modules = queries::round_modules(db, RoundKey::new(db, krate, at));
        for it in modules.stepping() {
            let module = modules.order[it as usize];
            crate_ops.take(&queries::macro_step(db, FactKey::new(db, module, krate, at)).ops);
        }
    }
    if crate_ops.data_changed {
        Arc::make_mut(&mut crate_ops.data).shrink_to_fit();
    }

    // One state per component, by its anchor in module order.
    let partition = queries::partition(db, round);
    let mut states: Vec<&'db SccState, &Frame> = Vec::with_capacity_in(partition.len() as usize, frame);
    for (at, &module) in (0u32..).zip(modules.order.iter()) {
        if partition.anchor(partition.of(at)) == at {
            states.push(queries::scc_state(db, SccKey::new(db, module, krate, r)));
        }
    }

    // The final round steps no module: each has the directives and tables of its last step, or
    // those it started with.
    let mut derive_helpers: FrameMap<'_, _, FrameVec<'_, _>> = map_in(frame);
    let mut pending: Vec<macros::Carried<'db>, &Frame> = Vec::new_in(frame);
    for (at, &module) in (0u32..).zip(modules.order.iter()) {
        let carried = macros::Carried::at(db, krate, modules, at);
        let tables = carried.tables();
        let SideTables { derive_helpers: helpers_of, deferred_derives, .. } = &*tables;
        always!(
            deferred_derives.is_empty(),
            "the deferred builtin derives of {module:?}: {deferred_derives:#?}"
        );
        for (&item, helpers) in helpers_of {
            // clone: the final tables are query values; each helper is copied into the map once.
            derive_helpers.entry(item).or_insert_with(|| Vec::new_in(frame)).extend(helpers.iter().cloned());
        }
        if modules.has_pending(at) {
            pending.push(carried);
        }
    }

    let env = queries::round_env(db, round).env(db);
    let local = env.local_def_map();
    let CrateOps { mut diagnostics, data, session_macros, unresolved_extern_crates, records, .. } =
        crate_ops;
    let mut def_map = DefMap {
        krate,
        block: None,
        root,
        modules: ModulesSlot::Building(table),
        prelude: None,
        macro_use_prelude: HashMap::default(),
        derive_helpers_in_scope: owned_helpers(&derive_helpers),
        session_macros,
        names: NameRecords::default(),
        // alloc: empty, allocates nothing: the list is made from the frame's once it is whole.
        diagnostics: Vec::new(),
        diagnostics_by_module: DiagnosticsByModule::default(),
        records: None,
        data,
    };
    env.install(&mut def_map);
    if let Some(prelude) = states.iter().find_map(|it| it.prelude) {
        def_map.prelude = Some(prelude);
    }

    let (mut def_map, mut local) = if pending.is_empty() {
        (def_map, local)
    } else {
        let cx = collect::cx(db, frame, krate, false);
        let mut sink = FinishSink { db, def_map, local, states: &states, diagnostics: &mut diagnostics };
        for carried in &pending {
            macros::unresolved_macro_diagnostics(&cx, &mut sink, carried.directives());
        }
        (sink.def_map, sink.local)
    };

    // Every import of the final round, in module then import order: a member's imports are
    // in its state in the order they were pushed, which is round order.
    let mut imports: Vec<(u32, &'db ImportOp, Option<PerNs>), &Frame> = Vec::new_in(frame);
    for state in &states {
        imports.extend(state.imports.iter().map(|(import, status)| {
            let at = *modules.index.get(&import.module).unwrap_or_else(|| {
                panic!("nameres facts: an import of {:?}, which is not a module of round {r}", import.module)
            });
            (at, import, *status)
        }));
    }
    imports.sort_by_key(|&(at, ..)| at);
    for &(_, import, status) in &imports {
        if status.is_some() || in_unresolved_extern_crate(&import.path, &unresolved_extern_crates) {
            continue;
        }
        diagnostics.push(DefDiagnostic::unresolved_import(
            import.module,
            import.source.id.lookup(db).id,
            import.source.use_tree,
        ));
    }
    // Then what rustc reports of imports of a trait's associated items.
    for &(_, import, status) in &imports {
        trait_item_import_diagnostics(db, &def_map, import, status, |it| diagnostics.push(it));
    }
    // alloc: the def map's own list, at exact size, once per def map.
    def_map.diagnostics = diagnostics.into_iter().collect();
    def_map.names = name_records(
        db,
        frame,
        &def_map,
        &local,
        imports.iter().map(|&(_, import, status)| (import, status)),
        records,
    );

    // Every scope is a handle already: this makes the `Modules` and groups the diagnostics.
    def_map.hand_over_scopes(db);
    def_map.shrink_to_fit();
    local.shrink_to_fit();
    (def_map, local)
}

/// What rustc's `finalize_imports` reports of `import`, resolved to `status` (`None` when it did
/// not resolve): a glob of what is not a module, enum or trait (`is_glob_of_non_module`), a
/// trait's items imported without `import_trait_associated_functions`, or a glob of a trait
/// whose items blaze cannot list (`DefMap::trait_item_import_reports`).
pub(crate) fn trait_item_import_diagnostics(
    db: &Db,
    def_map: &DefMap,
    import: &ImportOp,
    status: Option<PerNs>,
    mut push: impl FnMut(DefDiagnostic),
) {
    let ImportSource { use_tree, id, kind, .. } = import.source;
    let res = status.unwrap_or_else(PerNs::none);
    if is_glob_of_non_module(kind, res) {
        push(DefDiagnostic::unresolved_import(import.module, id.lookup(db).id, use_tree));
    }
    def_map.trait_item_import_reports(db, kind, res, |why| {
        push(DefDiagnostic::trait_item_import(import.module, id.lookup(db).id, use_tree, why));
    });
}

/// The crate-level ops of every log, in round then module order.
struct CrateOps<'f> {
    diagnostics: FrameVec<'f, DefDiagnostic>,
    data: Arc<DefMapCrateData>,
    data_changed: bool,
    session_macros: bool,
    unresolved_extern_crates: FrameSet<'f, Name>,
    records: Records<'f>,
}

impl CrateOps<'_> {
    fn take(&mut self, ops: &OpLog) {
        for op in ops.iter() {
            match op {
                DeclOp::Diagnostic(it) => self.diagnostics.push(copy_diagnostic(it)),
                DeclOp::ExportedDerive { mac, helpers } => {
                    self.data_changed = true;
                    // clone: a derive's helper names, once per derive the crate exports.
                    Arc::make_mut(&mut self.data).exported_derives.insert(*mac, helpers.clone());
                }
                &DeclOp::ProcMacroFn { func, mac } => {
                    self.data_changed = true;
                    let data = Arc::make_mut(&mut self.data);
                    data.fn_proc_macro_mapping.insert(func, mac);
                    data.fn_proc_macro_mapping_back.insert(mac, func);
                }
                DeclOp::SessionMacros => self.session_macros = true,
                &DeclOp::UnresolvedExternCrate { name } => {
                    self.unresolved_extern_crates.insert(name);
                }
                op => {
                    self.records.take(op);
                }
            }
        }
    }
}

/// `finish`'s rule: an import whose path starts with the name of an `extern crate` that did not
/// resolve gets no diagnostic of its own.
pub(crate) fn in_unresolved_extern_crate(path: &ModPath, unresolved: &FrameSet<'_, Name>) -> bool {
    matches!(
        (path.segments().first(), &path.kind),
        (Some(krate), PathKind::Plain | PathKind::Abs) if unresolved.contains(krate)
    )
}

/// The ops `NameRecords` is built from (the old `resolved_macro_paths`, `derive_helper_uses`
/// and `not_introducing`), in log order.
pub(crate) struct Records<'f> {
    macro_paths: FrameVec<'f, MacroPath>,
    derive_helpers: FrameVec<'f, DeriveHelperUse>,
    not_introducing: FrameVec<'f, ExternCrateId>,
}

/// The old `ResolvedMacroPath`.
struct MacroPath {
    module: ModuleId,
    path: ModPath,
    subns: MacroSubNs,
    def: MacroDefId,
    call: Option<MacroCallId>,
}

impl<'f> Records<'f> {
    pub(crate) fn new_in(frame: &'f Frame) -> Self {
        Records {
            macro_paths: Vec::new_in(frame),
            derive_helpers: Vec::new_in(frame),
            not_introducing: Vec::new_in(frame),
        }
    }

    /// Keeps `op` if it is one of the three, and says whether it was.
    pub(crate) fn take(&mut self, op: &DeclOp) -> bool {
        match *op {
            DeclOp::ResolvedMacroPath { module, path, bang, def, call } => {
                let subns = if bang { MacroSubNs::Bang } else { MacroSubNs::Attr };
                self.macro_paths.push(MacroPath { module, path, subns, def, call });
            }
            DeclOp::DeriveHelperUse(it) => self.derive_helpers.push(it),
            DeclOp::NotIntroducing(it) => self.not_introducing.push(it),
            _ => return false,
        }
        true
    }
}

/// The old `name_records`: each import and macro path resolved again on the def map as
/// collected, as rustc's `finalize_imports` and `finalize_macro_resolutions` do, into what
/// they went through. `imports` is every import with what it resolved to, `None` while
/// unresolved; `local` is the crate's `LocalDefMap`, a block's too.
pub(crate) fn name_records<'a>(
    db: &Db,
    frame: &Frame,
    def_map: &DefMap,
    local: &LocalDefMap,
    imports: impl Iterator<Item = (&'a ImportOp, Option<PerNs>)>,
    records: Records<'_>,
) -> NameRecords {
    let q = (local, db, ResolveMode::Import);
    let mut uses = Vec::new_in(frame);
    let mut found = Vec::new_in(frame);
    let mut unresolved = Vec::new_in(frame);
    let mut glob_targets = Vec::new_in(frame);
    for (import, status) in imports {
        let ImportSource { use_tree, id, kind, is_prelude } = import.source;
        let own = match kind {
            ImportKind::Glob => ImportOrExternCrate::Glob(GlobId { use_: id, idx: use_tree }),
            ImportKind::Plain | ImportKind::TypeOnly => {
                ImportOrExternCrate::Import(ImportId { use_: id, idx: use_tree })
            }
        };
        let Some(namespaces) = status else {
            unresolved.push(own);
            continue;
        };
        let ns = match kind {
            ImportKind::Plain => None,
            ImportKind::Glob | ImportKind::TypeOnly => Some(Namespace::Types),
        };
        let shadow = BuiltinShadowMode::Module;
        def_map.path_uses(q, import.module, &import.path, shadow, None, ns, true, &mut found);
        uses.extend(found.drain(..).filter(|it: &Use| it.import != own));
        if kind == ImportKind::Glob
            && !is_prelude
            && let Some(ModuleDefId::ModuleId(target)) = namespaces.take_types()
            && target.krate(db) == def_map.krate
            && let ImportOrExternCrate::Glob(glob) = own
        {
            glob_targets.push((glob, target));
        }
    }
    let mut macro_calls = Vec::new_in(frame);
    for it in &records.macro_paths {
        let MacroPath { module, ref path, subns, def, call } = *it;
        let Some((macro_id, via)) = def_map.macro_via(local, db, module, path, subns, def) else {
            continue;
        };
        if let Some(call) = call {
            macro_calls.push(MacroCallRecord { call, def: macro_id, via });
        }
        if via != MacroVia::Textual {
            let subns = Some(subns);
            def_map.path_uses_outside_imports(
                local,
                db,
                module,
                path,
                Namespace::Macros,
                subns,
                true,
                &mut found,
            );
            uses.extend(found.drain(..));
        }
    }
    let mut seen = set_in(frame);
    uses.retain(|&it| seen.insert(it));
    NameRecords {
        // alloc: the records' own lists, at exact size, once per def map.
        uses: uses.into_iter().collect(),
        // alloc: as `uses`.
        unresolved: unresolved.into_iter().collect(),
        // alloc: as `uses`.
        glob_targets: glob_targets.into_iter().collect(),
        // alloc: as `uses`.
        macro_calls: macro_calls.into_iter().collect(),
        // alloc: as `uses`.
        derive_helpers: records.derive_helpers.into_iter().collect(),
        // alloc: as `uses`.
        not_introducing: records.not_introducing.into_iter().collect(),
    }
}

/// The def map's derive helpers, from a run's frame tables: the table and each list at exact
/// size.
fn owned_helpers(helpers: &FrameMap<'_, AstId<ast::Item>, FrameVec<'_, DeriveHelper>>) -> DeriveHelpers {
    // alloc: the def map's table, at its size, once per def map.
    let mut out = DeriveHelpers::with_capacity_and_hasher(helpers.len(), Default::default());
    // alloc: each item's list, at exact size, once per def map; clone: each helper once.
    out.extend(helpers.iter().map(|(&item, list)| (item, list.to_vec())));
    out
}

/// A copy of a diagnostic of a log (a query value) for the def map, which owns its list.
pub(crate) fn copy_diagnostic(diagnostic: &DefDiagnostic) -> DefDiagnostic {
    use DefDiagnosticKind as K;
    // clone: the diagnostic's own data (a cfg expression, a message), once per diagnostic.
    let kind = match &diagnostic.kind {
        K::UnresolvedModule { ast, candidates } => {
            K::UnresolvedModule { ast: *ast, candidates: candidates.clone() } // clone: see above.
        }
        K::UnresolvedExternCrate { ast } => K::UnresolvedExternCrate { ast: *ast },
        K::UnresolvedImport { id, index } => K::UnresolvedImport { id: *id, index: *index },
        K::TraitItemImport { id, index, why } => K::TraitItemImport { id: *id, index: *index, why: *why },
        K::UnconfiguredCode { ast_id, cfg } => {
            K::UnconfiguredCode { ast_id: *ast_id, cfg: cfg.clone() } // clone: see above.
        }
        K::UnresolvedMacroCall { ast, path } => {
            K::UnresolvedMacroCall { ast: ast.clone(), path: *path } // clone: see above.
        }
        K::UnimplementedBuiltinMacro { ast } => K::UnimplementedBuiltinMacro { ast: *ast },
        K::InvalidDeriveTarget { ast, id } => K::InvalidDeriveTarget { ast: *ast, id: *id },
        K::MalformedDerive { ast, id } => K::MalformedDerive { ast: *ast, id: *id },
        // clone: see above.
        K::MacroDefError { ast, message } => K::MacroDefError { ast: *ast, message: message.clone() },
        // clone: see above.
        K::MacroError { ast, path, err } => K::MacroError { ast: *ast, path: *path, err: err.clone() },
        K::Prelude { krate, error } => K::Prelude { krate: *krate, error: *error },
    };
    DefDiagnostic { in_module: diagnostic.in_module, kind }
}

/// The sink of `finish`'s unresolved-macro diagnostics: the assembled def map, which their
/// paths resolve against as the old `finish` resolved them against the final map. It takes
/// diagnostics only, into the def map's list being built.
struct FinishSink<'a, 'db, 'f> {
    db: &'db Db,
    def_map: DefMap,
    local: LocalDefMap,
    states: &'a [&'db SccState],
    diagnostics: &'a mut FrameVec<'f, DefDiagnostic>,
}

impl<'db> Sink<'db> for FinishSink<'_, 'db, '_> {
    fn db(&self) -> &'db Db {
        self.db
    }

    fn def_map(&self) -> &DefMap {
        &self.def_map
    }

    fn local_def_map(&self) -> &LocalDefMap {
        &self.local
    }

    fn apply(&mut self, op: DeclOp) {
        match op {
            DeclOp::Diagnostic(it) => self.diagnostics.push(it),
            op => panic!("nameres facts: finish's unresolved-macro diagnostics made {op:?}"),
        }
    }

    fn find_unresolved_named<T>(
        &self,
        module: ModuleId,
        name: Name,
        mut f: impl FnMut(&ModPath) -> Option<T>,
    ) -> Option<T> {
        self.states
            .iter()
            .flat_map(|state| state.imports.iter())
            .filter(|(_, status)| status.is_none())
            .map(|(it, _)| it)
            .filter(|it| {
                it.module == module
                    && matches!(it.source.kind, ImportKind::Plain)
                    && it.bound_name() == Some(name)
            })
            .find_map(|it| f(&it.path))
    }

    fn child(&mut self, request: ChildRequest<'_>) -> ChildMode {
        panic!("nameres facts: finish's unresolved-macro diagnostics declared {:?}", request.child)
    }

    fn take_macros(&mut self, into: &mut FrameVec<'_, MacroOp>) {
        // `apply` takes no directive, so there is none to move.
        let _ = into;
    }
}

// A block.

/// A block's def map from the empty one `block_def_map` makes: the old `collect_defs` on one
/// unit holding every module of the block, whose upstream (the crate's map and the blocks
/// around) is final.
pub(crate) fn collect_block<'db>(
    db: &'db Db,
    frame: &Frame,
    def_map: DefMap,
    tree_id: TreeId,
    local: &'db LocalDefMap,
) -> DefMap {
    let krate = def_map.krate;
    let cx = collect::cx(db, frame, krate, true);
    let mut sink = BlockSink { unit: Unit::block(&cx, def_map, local), records: Records::new_in(frame) };
    let mut side: SideTablesIn<'_> = SideTables::new_in(frame);
    let mut pending: FrameVec<'_, MacroOp> = Vec::new_in(frame);
    collect::seed_block(&cx, &mut sink, &mut side, tree_id);
    sink.take_macros(&mut pending);

    // The old `resolution_loop`.
    let mut rounds = 0;
    let mut eager_with_errors = false;
    'resolve_attr: loop {
        'resolve_macros: loop {
            blaze_query::db::unwind_if_cancelled(db);
            sink.unit.set_unexpanded(!pending.is_empty());
            sink.unit.resolve_imports();
            let result =
                macros::resolve_round(&cx, &mut sink, &mut side, &mut pending, eager_with_errors);
            if !result.progress {
                // What is left may be eager calls waiting for a macro of their arguments: now
                // nothing else can define it, they are expanded with the error, only when this
                // round left one for later.
                if eager_with_errors || !result.deferred_eager {
                    break 'resolve_macros;
                }
                eager_with_errors = true;
                continue 'resolve_macros;
            }
            eager_with_errors = false;
            rounds += 1;
            if rounds > FIXED_POINT_LIMIT {
                let imports = sink.unit.import_outcomes().filter(|(_, it)| it.is_none()).map(|(it, _)| it);
                fixpoint_limit(db, krate, true, rounds, pending.iter(), imports).throw();
            }
        }
        let Some(index) = macros::first_attr(&pending) else { break 'resolve_attr };
        macros::reseed_at(&cx, &mut sink, &mut side, &mut pending, index);
        eager_with_errors = false;
    }

    // The old `collect`'s end.
    sink.unit.record_unresolved();
    if cx.is_proc_macro {
        let def_map = sink.unit.def_map_mut();
        let root = def_map.root;
        def_map.modules.building()[root].scope_mut().censor_non_proc_macros(krate);
    }

    // The old `finish`.
    macros::unresolved_macro_diagnostics(&cx, &mut sink, &pending);
    let BlockSink { mut unit, records } = sink;
    // The block's diagnostics, then its unresolved imports, read before the unit gives up its
    // map.
    let mut diagnostics = unit.take_diagnostics();
    diagnostics.extend(
        unit.unresolved_imports()
            .filter(|it| !in_unresolved_extern_crate(&it.path, unit.unresolved_extern_crates()))
            .map(|it| DefDiagnostic::unresolved_import(it.module, it.source.id.lookup(db).id, it.source.use_tree)),
    );
    for (import, status) in unit.import_outcomes() {
        trait_item_import_diagnostics(db, unit.def_map(), import, status, |it| diagnostics.push(it));
    }
    let names = name_records(db, frame, unit.def_map(), local, unit.import_outcomes(), records);
    let mut def_map = unit.into_def_map();
    // alloc: the def map's own list, at exact size, once per block.
    def_map.diagnostics = diagnostics.into_iter().collect();
    def_map.names = names;
    always!(
        side.deferred_derives.is_empty(),
        "the deferred builtin derives of a block: {:#?}",
        side.deferred_derives
    );
    def_map.derive_helpers_in_scope = owned_helpers(&side.derive_helpers);
    def_map.shrink_to_fit();
    def_map.hand_over_scopes(db);
    def_map
}

/// A block's unit, keeping the ops `NameRecords` is built from, which the unit does not hold.
struct BlockSink<'db, 'f> {
    unit: Unit<'db, 'f, &'f Frame>,
    records: Records<'f>,
}

impl<'db> Sink<'db> for BlockSink<'db, '_> {
    fn db(&self) -> &'db Db {
        self.unit.db()
    }

    fn def_map(&self) -> &DefMap {
        self.unit.def_map()
    }

    fn local_def_map(&self) -> &LocalDefMap {
        self.unit.local_def_map()
    }

    fn apply(&mut self, op: DeclOp) {
        if !self.records.take(&op) {
            self.unit.apply(op);
        }
    }

    fn find_unresolved_named<T>(
        &self,
        module: ModuleId,
        name: Name,
        f: impl FnMut(&ModPath) -> Option<T>,
    ) -> Option<T> {
        self.unit.find_unresolved_named(module, name, f)
    }

    fn child(&mut self, request: ChildRequest<'_>) -> ChildMode {
        self.unit.child(request)
    }

    fn take_macros(&mut self, into: &mut FrameVec<'_, MacroOp>) {
        self.unit.take_macros(into);
    }

    fn expanded_now(&mut self, module: ModuleId, call: MacroCallId) {
        self.unit.expanded_now(module, call);
    }
}

#[cfg(test)]
mod tests {
    use base_db::Crate;
    use hir_expand::name::Name;

    use super::super::{
        queries::{self, RoundKey, RoundKind},
        tests::{
            Exempt, compare, compare_fixture, fixture_db, new_block_def_map,
            new_crate_local_def_map, old_block_def_map, old_crate_local_def_map, project,
        },
    };
    use crate::{
        DefWithBodyId, ModuleDefId,
        expr_store::Body,
        nameres::{
            ModuleOrigin, crate_local_def_map,
            uses::{NameRecords, Used},
        },
        per_ns::PerNs,
        test_db::{TestDB, TestDBExt},
    };

    /// Each round's kind until the final one.
    fn round_kinds(db: &TestDB, krate: Crate) -> Vec<RoundKind> {
        let mut kinds = Vec::new();
        for round in 0..64 {
            let kind = queries::round_kind(db, RoundKey::new(db, krate, round));
            kinds.push(kind);
            if kind == RoundKind::Final {
                return kinds;
            }
        }
        panic!("no final round in 64: {kinds:?}");
    }

    fn assert_same_as_old(name: &str, fixture: &str) {
        let mut out = String::new();
        compare_fixture(name, fixture, &Exempt::NONE, &mut out);
        assert!(out.is_empty(), "{out}");
    }

    #[test]
    fn round_zero_order_is_the_old_insertion_order() {
        let db = fixture_db(
            r#"
//- /lib.rs
mod a;
mod b {
    mod c {}
    pub mod d;
}
mod e;
//- /a.rs
mod inner {}
//- /b/d.rs
pub struct S;
//- /e.rs
"#,
        );
        let krate = db.fetch_test_crate();
        let old = old_crate_local_def_map(&db, krate).def_map(&db);
        let old: Vec<ModuleOrigin> = old.modules(&db).map(|(_, data)| data.origin).collect();
        let new: Vec<ModuleOrigin> = queries::round_modules(&db, RoundKey::new(&db, krate, 0))
            .rows
            .iter()
            .map(|row| row.origin)
            .collect();
        assert_eq!(new.len(), 7, "{new:#?}");
        assert_eq!(old, new);
    }

    #[test]
    fn import_shape_of_round_zero() {
        let db = fixture_db(
            r#"
//- /lib.rs
mod a {
    pub struct X;
    pub mod inner {
        pub(in crate::a) use super::X as Y;
    }
}
use a::X;
pub(crate) use a::*;
use a::inner as renamed;
extern crate self as me;
"#,
        );
        let krate = db.fetch_test_crate();
        let shape = queries::import_shape(&db, RoundKey::new(&db, krate, 0));
        assert_eq!(shape.modules.len(), 3);
        let root = &shape.modules[0];
        assert_eq!(root.parent, None);
        assert_eq!(shape.modules[1].parent, Some(0));
        assert_eq!(shape.modules[2].parent, Some(1));
        assert!(root.local_names.contains(&(Name::new_root("a"), 1)), "{:?}", root.local_names);
        assert!(root.local_names.contains(&(Name::new_root("me"), 0)), "{:?}", root.local_names);
        assert!(shape.modules[1].local_names.contains(&(Name::new_root("inner"), 2)));
        let binds: Vec<(Option<Name>, bool)> =
            root.imports.iter().map(|it| (it.binds, it.is_glob)).collect();
        assert_eq!(
            binds,
            [(Some(Name::new_root("X")), false), (None, true), (Some(Name::new_root("renamed")), false)]
        );
        assert!(root.other_paths.is_empty(), "{:?}", root.other_paths);
        assert_eq!(shape.modules[2].other_paths.len(), 1, "{:?}", shape.modules[2].other_paths);
        assert_eq!(shape.modules[2].imports.len(), 1);
        assert!(shape.extern_prelude_self.contains(&Name::new_root("me")));
    }

    #[test]
    fn round_kinds_progress() {
        let fixture = r#"
//- /lib.rs
mod m {
    #[macro_export]
    macro_rules! make { () => { pub struct Made; } }
}
crate::make!();
"#;
        let db = fixture_db(fixture);
        // The old loop: `resolve_macros` resolves the path call (`No`), then nothing is left
        // (`Yes`, no deferred eager call, no attribute).
        assert_eq!(
            round_kinds(&db, db.fetch_test_crate()),
            [RoundKind::Normal, RoundKind::Normal, RoundKind::Final]
        );
        assert_same_as_old("round_kinds_progress", fixture);
    }

    /// The round count of a macro chain behind a glob: the call the expansion makes resolves
    /// there and expands within the same round, so one round resolves the whole chain and the
    /// next finds nothing, never a round rescheduled with nothing pending.
    #[test]
    fn round_kinds_of_a_macro_chain_through_a_glob() {
        let fixture = r#"
//- /lib.rs
mod a {
    #[macro_export]
    macro_rules! outer { () => { $crate::inner!(); } }
    #[macro_export]
    macro_rules! inner { () => { pub struct Made; } }
}
mod b {
    pub use crate::*;
}
use b::Made;
outer!();
"#;
        let db = fixture_db(fixture);
        let krate = db.fetch_test_crate();
        assert_eq!(round_kinds(&db, krate), [RoundKind::Normal, RoundKind::Normal, RoundKind::Final]);
        let def_map = crate_local_def_map(&db, krate).def_map(&db);
        let scope = def_map.module(&db, def_map.root).scope(&db);
        assert_ne!(scope.get(&Name::new_root("Made")), PerNs::none(), "`Made` through the glob");
        assert_same_as_old("round_kinds_of_a_macro_chain_through_a_glob", fixture);
    }

    #[test]
    fn round_kinds_eager_with_errors() {
        let fixture = r#"
//- /lib.rs
#[rustc_builtin_macro]
macro_rules! concat {}

concat!(not_defined!());
"#;
        let db = fixture_db(fixture);
        // The old loop: the eager call is deferred (`Yes` with a deferred call), so the next
        // round expands it with the error (`No`), then a normal round finds nothing (`Yes`, no
        // deferred call), and with no attribute left the loop ends.
        assert_eq!(
            round_kinds(&db, db.fetch_test_crate()),
            [RoundKind::Normal, RoundKind::EagerWithErrors, RoundKind::Normal, RoundKind::Final]
        );
        assert_same_as_old("round_kinds_eager_with_errors", fixture);
    }

    #[test]
    fn round_kinds_unresolved_attribute() {
        let fixture = r#"
//- /lib.rs
#[unresolved]
struct Foo;
"#;
        let db = fixture_db(fixture);
        let krate = db.fetch_test_crate();
        let root = queries::crate_root(&db, krate);
        // The old loop: the attribute does not resolve (`Yes`, nothing deferred), so
        // `reseed_with_unresolved_attribute` collects `Foo` (`No`), then nothing is left.
        assert_eq!(
            round_kinds(&db, krate),
            [
                RoundKind::Normal,
                RoundKind::Reseed { module: root, index: 0 },
                RoundKind::Normal,
                RoundKind::Final
            ]
        );
        assert_same_as_old("round_kinds_unresolved_attribute", fixture);
    }

    #[test]
    fn collect_crate_equals_the_old_collector() {
        assert_same_as_old(
            "collect_crate_equals_the_old_collector",
            r#"
//- /lib.rs
mod a;
pub mod b {
    pub use super::a::*;
    pub struct B;
}
use b::{A, B};
pub use self::c::C as Renamed;
mod c {
    pub struct C;
    pub fn f() {}
    pub(super) use super::b::B as CB;
}
macro_rules! m { () => { pub struct FromMacro; } }
m!();
crate::exported!();
use self::missing::Nothing;
//- /a.rs
pub struct A;
pub mod deep {
    pub use crate::c::*;
}
#[macro_export]
macro_rules! exported { () => { pub struct Exported; } }
"#,
        );
    }

    /// The old collector settles glob ambiguities when collection ends; a crate's scopes are
    /// handed over by `scc_state`, which must settle them itself at the final round.
    #[test]
    fn glob_ambiguities_are_settled_as_the_old_collector_settles_them() {
        assert_same_as_old(
            "glob_ambiguities_are_settled_as_the_old_collector_settles_them",
            r#"
//- /lib.rs
mod a { pub struct X; }
mod b { pub struct X; }
mod c { pub struct X; }
use a::*;
use b::*;
use c::X;
mod still {
    pub use crate::a::*;
    pub use crate::b::*;
}
"#,
        );
    }

    #[test]
    fn collect_block_equals_the_old_block_path() {
        let fixture = r#"
//- /lib.rs
pub mod outer {
    pub struct Outer;
}
fn f() {
    mod inner {
        pub struct Inner;
    }
    use inner::Inner;
    use crate::outer::Outer;
    use self::missing::Nothing;
    struct Local;
    unknown!();
}
"#;
        let db = fixture_db(fixture);
        let krate = db.fetch_test_crate();
        let def_map = crate_local_def_map(&db, krate).def_map(&db);
        let f = def_map
            .module(&db, def_map.root)
            .scope(&db)
            .declarations()
            .find_map(|it| match it {
                ModuleDefId::FunctionId(it) => Some(it),
                _ => None,
            })
            .expect("the fixture's `fn f`");
        let body = Body::of(&db, DefWithBodyId::from(f));
        let blocks: Vec<_> = body.blocks(&db).map(|(block, _)| block).collect();
        assert_eq!(blocks.len(), 1);
        let block = blocks[0].to_db(&db);
        let old = old_block_def_map(&db, block);
        let new = new_block_def_map(&db, block);
        let diff =
            compare(&project(&db, &old, None, &Exempt::NONE), &project(&db, &new, None, &Exempt::NONE));
        assert!(diff.is_empty(), "{diff}");
        let scope = new.module(&db, new.root).scope(&db);
        for name in ["inner", "Inner", "Outer", "Local"] {
            assert_ne!(scope.get(&Name::new_root(name)), PerNs::none(), "`{name}` in the block");
        }
        assert!(!new.diagnostics(&db).is_empty(), "the unresolved import and macro are diagnosed");
        assert_eq!(records(old.name_records(&db)), records(new.name_records(&db)), "the block's name records");
    }

    /// What two collections' `NameRecords` must agree on without their ids, which differ: each
    /// use by name, namespace and how it was used, sorted; and the size of every other list.
    fn records(names: &NameRecords) -> (Vec<(String, String, Used)>, [usize; 5]) {
        let mut uses: Vec<_> = names
            .uses
            .iter()
            .map(|it| (it.name.as_str().to_owned(), format!("{:?}", it.ns), it.used))
            .collect();
        uses.sort();
        let lens = [
            names.unresolved.len(),
            names.glob_targets.len(),
            names.macro_calls.len(),
            names.derive_helpers.len(),
            names.not_introducing.len(),
        ];
        (uses, lens)
    }

    #[test]
    fn name_records_equal_the_old_ones() {
        let db = fixture_db(
            r#"
//- /lib.rs
mod a {
    pub struct X;
    pub fn x() {}
    pub mod inner {
        pub struct Y;
    }
    #[macro_export]
    macro_rules! exported { () => { pub struct Exported; } }
}
use a::X;
use a::inner::*;
pub use self::a::x as renamed;
use self::renamed as twice;
use self::missing::Nothing;
crate::exported!();
macro_rules! local { () => {} }
local!();
"#,
        );
        let krate = db.fetch_test_crate();
        let old = old_crate_local_def_map(&db, krate);
        let new = new_crate_local_def_map(&db, krate);
        let old = records(old.def_map(&db).name_records(&db));
        let new = records(new.def_map(&db).name_records(&db));
        assert_eq!(old, new);
        assert!(!new.0.is_empty(), "the fixture's imports go through bindings: {new:?}");
        assert_eq!(new.1[0], 1, "one unresolved import");
        assert_eq!(new.1[1], 1, "one glob of a local module");
        assert_eq!(new.1[2], 2, "two macro calls");
    }
}
