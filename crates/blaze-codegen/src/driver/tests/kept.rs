//! The walk over kept describes ([`Build::keeping`], docs/v2/plan/design/incremental-walk.md)
//! against the walk that keeps none: after any edit the two build the same program, and an
//! unchanged function costs the front end's answer, never a describe.
//!
//! The front end here is [`Program`] with its [`Engine`]: every edit is a revision, and a kept
//! describe holds when it was made at or after the last change of what it reads, as a query
//! engine verifies a memo. The fresh build is of a copy of the program in a database of its
//! own ([`Program::copy`]), so nothing the kept walk did is seen by it.

use std::sync::atomic::Ordering;

use super::*;

/// What a build of the tests walks from, beside `main` (function 1).
#[derive(Clone, Copy, Debug)]
struct Run {
    /// A live build, whose edited code is reached through slots ([`rebind`]).
    live: bool,
    /// Static 1 is a root: built though no code names it.
    static_root: bool,
    /// A root of the recursion limit's check that the build may not reach: described for
    /// the check, built only where a root of the build reaches it.
    check: Option<u32>,
}

/// One edit of a program between two builds. Each names functions by two draws, taken among
/// the functions the program has when it is applied.
#[derive(Clone, Copy, Debug)]
enum Edit {
    /// A function's body.
    Body,
    /// A function calls one more function.
    AddRef,
    /// A function calls one function fewer.
    RemoveRef,
    /// A new function, called by one there is.
    AddFunction,
    /// A function goes, with every reference to it.
    RemoveFunction,
    /// A static's value.
    Static,
    /// The function a call through a `dyn` is made direct to: a dyn fact of the program.
    DynFact,
    /// The recursion limit.
    Limit,
    /// A function names a static it did not, or stops naming one it did.
    StaticRef,
    /// A static's value points to a function it did not, or stops pointing to one.
    StaticCall,
    /// The build's roots: whether static 1 is one, and which function the check walks from.
    Roots,
}

const EDITS: [Edit; 11] = [
    Edit::Body,
    Edit::AddRef,
    Edit::RemoveRef,
    Edit::AddFunction,
    Edit::RemoveFunction,
    Edit::Static,
    Edit::DynFact,
    Edit::Limit,
    Edit::StaticRef,
    Edit::StaticCall,
    Edit::Roots,
];

/// The safeguard a control takes away.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    /// The driver takes a kept describe without asking the front end
    /// ([`Build::trusting_kept`]).
    Unasked,
    /// The engine sees no change of a function whose reference was removed.
    RemovedRef,
    /// The engine sees no change of a dyn fact ([`Engine::misses_facts`]).
    DynFact,
    /// The driver patches a changed function whatever its shape
    /// ([`Build::patching_any_shape`]).
    AnyShape,
    /// The driver leaves the places that named a changed function's old key
    /// ([`Build::without_renaming`]).
    Unrenamed,
    /// A bug planted in the patch of the walk or of the lists ([`Planted`]).
    Patch(Planted),
}

/// The program's functions, in order: a draw picks among them the same way every run.
fn ids(program: &Program) -> Vec<u32> {
    let mut ids: Vec<u32> = program.functions.keys().copied().collect();
    ids.sort_unstable();
    ids
}

/// Takes the reference at `at` of `caller`'s calls away. The engine sees the change unless
/// the control plants that it does not.
fn remove_ref(program: &mut Program, caller: u32, at: usize, plant: Option<Plant>) {
    program.functions.get_mut(&caller).expect("a function of the program").calls.remove(at);
    if plant != Some(Plant::RemovedRef) {
        program.touch(caller);
    }
}

/// Makes the call through a `dyn` of `site` direct to `target`.
fn set_fact(program: &mut Program, site: u32, target: u32) {
    program.dyn_sites.insert(site, target);
    program.touch_fact(site);
}

/// Applies `edit` to `program`, `(a, b)` picking what of it: every change of what a describe
/// reads is a revision of the engine's. `kept` are functions that stay: `main` and the check's
/// root.
fn apply(
    program: &mut Program,
    edit: Edit,
    (a, b): (u32, u32),
    kept: &[u32],
    plant: Option<Plant>,
) {
    let ids = ids(program);
    let pick = |draw: u32| ids[draw as usize % ids.len()];
    match edit {
        Edit::Body => {
            let id = pick(a);
            let function = program.functions.get_mut(&id).expect("picked among them");
            // Never the body whose IR can't be built: a build's error is its own test.
            function.body = match function.body.wrapping_add(1 + (b % 5) as u8) {
                FAILS_ITS_CHECK => FAILS_ITS_CHECK + 1,
                body => body,
            };
            program.touch(id);
        }
        Edit::AddRef => {
            let (caller, callee) = (pick(a), pick(b));
            let function = program.functions.get_mut(&caller).expect("picked among them");
            if !function.calls.contains(&callee) {
                function.calls.push(callee);
                program.touch(caller);
            }
        }
        Edit::RemoveRef => {
            let caller = pick(a);
            let calls = program.functions[&caller].calls.len();
            if calls > 0 {
                remove_ref(program, caller, b as usize % calls, plant);
            }
        }
        Edit::AddFunction => {
            let (caller, id) = (pick(a), ids.last().expect("`main` at least") + 1);
            let function = program.function(id, 0x10 + (id % 0x40) as u8, &[]);
            if b % 3 == 0 {
                function.origin = Origin::Dependency(Package::new("dep"));
            }
            program.bodies.insert(id, (id % 3, false));
            program.touch(id);
            program.functions.get_mut(&caller).expect("picked among them").calls.push(id);
            program.touch(caller);
        }
        Edit::RemoveFunction => {
            let id = pick(a);
            if kept.contains(&id) {
                return;
            }
            program.functions.remove(&id);
            program.bodies.remove(&id);
            for caller in ids.iter().copied().filter(|it| *it != id) {
                let function = program.functions.get_mut(&caller).expect("still there");
                let before = function.calls.len();
                function.calls.retain(|it| *it != id);
                if function.calls.len() != before {
                    program.touch(caller);
                }
            }
            for calls in program.static_calls.values_mut() {
                calls.retain(|it| *it != id);
            }
            let mut sites: Vec<u32> = (program.dyn_sites.iter())
                .filter(|(site, target)| **site == id || **target == id)
                .map(|(site, _)| *site)
                .collect();
            sites.sort_unstable();
            for site in sites {
                program.dyn_sites.remove(&site);
                program.touch_fact(site);
            }
        }
        Edit::Static => {
            let mut statics: Vec<u32> = program.statics.keys().copied().collect();
            statics.sort_unstable();
            if !statics.is_empty() {
                let id = statics[a as usize % statics.len()];
                let value = &mut program.statics.get_mut(&id).expect("picked among them").0;
                *value = value.wrapping_add(1 + (b % 5) as u8);
            }
        }
        Edit::DynFact => set_fact(program, pick(a), pick(b)),
        Edit::StaticRef => {
            let mut statics: Vec<u32> = program.statics.keys().copied().collect();
            statics.sort_unstable();
            if !statics.is_empty() {
                let (id, value) = (pick(a), statics[b as usize % statics.len()]);
                let named = &mut program.functions.get_mut(&id).expect("picked among them").statics;
                match named.iter().position(|it| *it == value) {
                    Some(at) => {
                        named.remove(at);
                    }
                    None => named.push(value),
                }
                program.touch(id);
            }
        }
        Edit::StaticCall => {
            let mut statics: Vec<u32> = program.statics.keys().copied().collect();
            statics.sort_unstable();
            if !statics.is_empty() {
                let (value, callee) = (statics[a as usize % statics.len()], pick(b));
                let calls = program.static_calls.entry(value).or_default();
                match calls.iter().position(|it| *it == callee) {
                    Some(at) => {
                        calls.remove(at);
                    }
                    None => calls.push(callee),
                }
            }
        }
        // The roots are the run's: `first_difference` changes them.
        Edit::Roots => {}
        Edit::Limit => {
            // The limit is every function's crate's: a durable input.
            program.engine.high += 1;
            program.recursion_limit = match b % 4 {
                0 => None,
                limit => Some(limit),
            };
        }
    }
}

/// The build of `program` as `run` says, its walk taking the describes `kept` holds, or none.
fn built(
    program: &Program,
    run: Run,
    kept: Option<&Kept<u32, u32>>,
    plant: Option<Plant>,
) -> Result<Built, Vec<CodegenError>> {
    let mode = if run.live { Mode::Live } else { Mode::Output };
    let policy = policy(OptLevel::O0, mode);
    let (machine, memo, store) = (Machine::default(), Memo::new(), Store::default());
    let static_roots: &[u32] = if run.static_root { &[1] } else { &[] };
    let check: Vec<CheckRoot<u32, u32>> =
        run.check.map(|id| CheckRoot::Function(id, member())).into_iter().collect();
    let build = Build::new(&machine, program, &policy, &memo);
    let build = match kept {
        Some(kept) => build.keeping(kept),
        None => build,
    };
    let mut build = match plant {
        Some(Plant::Unasked) => build.trusting_kept(),
        Some(Plant::AnyShape) => build.patching_any_shape(),
        Some(Plant::Unrenamed) => build.without_renaming(),
        Some(Plant::Patch(bug)) => build.planting(bug),
        _ => build,
    };
    build.run_with_statics(&[(1, member())], static_roots, &check, &store)
}

/// The oracle: the first thing `kept`, the build over kept describes, holds other than
/// `fresh`, the build of the same program over none. Everything the walk decides: which
/// functions and statics are built, each one's code and what its places name, the symbols,
/// the slots, the owners, what dedupe merged, what each root reaches, and a refused build's
/// errors. What a build counts of its compiles and the store is not the walk's.
fn difference(
    kept: &Result<Built, Vec<CodegenError>>,
    fresh: &Result<Built, Vec<CodegenError>>,
) -> Option<String> {
    let (kept, fresh) = match (kept, fresh) {
        (Ok(kept), Ok(fresh)) => (kept, fresh),
        (Err(kept), Err(fresh)) if kept == fresh => return None,
        (Err(kept), Err(fresh)) => {
            return Some(format!(
                "the errors differ: {kept:?} over kept describes, {fresh:?} fresh"
            ));
        }
        (Ok(_), Err(fresh)) => {
            return Some(format!("built over kept describes, refused fresh: {fresh:?}"));
        }
        (Err(kept), Ok(_)) => {
            return Some(format!("refused over kept describes, built fresh: {kept:?}"));
        }
    };
    macro_rules! same {
        ($($field:ident).+) => {
            if kept.$($field).+ != fresh.$($field).+ {
                return Some(format!(
                    "`{}` differs: {:?} over kept describes, {:?} fresh",
                    stringify!($($field).+),
                    kept.$($field).+,
                    fresh.$($field).+,
                ));
            }
        };
    }
    same!(debug_names);
    same!(inputs);
    same!(roots);
    same!(functions);
    same!(statics);
    same!(symbols);
    same!(static_symbols);
    same!(patchable);
    same!(tiered);
    same!(deduped);
    same!(items);
    same!(reach);
    same!(owners);
    same!(static_owners);
    same!(editable);
    same!(versions);
    same!(digests);
    same!(identities);
    same!(cold);
    same!(made);
    same!(walked);
    same!(entry);
    // The ID is of the lists alone: a build with nothing kept gives the same one.
    same!(id);
    same!(stats.instances);
    same!(stats.statics);
    same!(walk.reached);
    // Every function a round of a whole walk reached was described or taken from the kept
    // walk, once; a patched walk asks only of what is not durable.
    if kept.walk.whole.is_some()
        && kept.walk.described + kept.walk.verified != fresh.walk.described
    {
        return Some(format!(
            "the walk over kept describes asked for {} describes and took {} kept, where the \
             fresh walk described {}",
            kept.walk.described, kept.walk.verified, fresh.walk.described
        ));
    }
    None
}

/// Builds `program`, then builds again after each of `edits`, each build twice: over the
/// describes the builds before kept, and fresh, of a copy of the program in a database of
/// its own. The first difference between the two, naming the build.
fn first_difference(
    mut program: Program,
    run: Run,
    edits: &[(Edit, (u32, u32))],
    plant: Option<Plant>,
) -> Option<String> {
    program.engine.misses_facts = plant == Some(Plant::DynFact);
    let kept = Kept::new();
    let mut run = run;
    // The functions that stay: `main`, and every function the check may walk from.
    let mut stays: Vec<u32> = [1].into_iter().chain(run.check).collect();
    // The last build over kept describes of each program (the roots it is built from) that
    // was not refused: the base of that program's next delta. A refused build keeps
    // nothing, so the build after it has no base.
    let mut bases: Vec<((bool, Option<u32>), Option<Built>)> = Vec::new();
    for build in 0..=edits.len() {
        if let Some((edit, picks)) = build.checked_sub(1).map(|at| edits[at]) {
            apply(&mut program, edit, picks, &stays, plant);
            if matches!(edit, Edit::Roots) {
                let ids = ids(&program);
                run.static_root = program.statics.contains_key(&1) && picks.0 % 2 == 0;
                run.check = (picks.1 % 3 != 0).then(|| ids[picks.1 as usize % ids.len()]);
                stays.extend(run.check);
            }
        }
        program.begin_build();
        let over_kept = built(&program, run, Some(&kept), plant);
        let fresh = built(&program.copy(), run, None, None);
        if let Some(what) = difference(&over_kept, &fresh) {
            return Some(format!("build {build}: {what}"));
        }
        let of = (run.static_root, run.check);
        let at = match bases.iter().position(|it| it.0 == of) {
            Some(at) => at,
            None => {
                bases.push((of, None));
                bases.len() - 1
            }
        };
        match over_kept {
            Ok(now) => {
                let base = bases[at].1.as_ref();
                if let Some(what) = delta_differs(base, &now).or_else(|| unapplied(base, &now)) {
                    return Some(format!("build {build}: {what}"));
                }
                bases[at].1 = Some(now);
            }
            Err(_) => bases[at].1 = None,
        }
    }
    None
}

/// The oracle of a delta's use: `base`'s lists with `now.delta` applied are `now`'s. The
/// keys of `base` without those removed and with those added are `now`'s keys, and every
/// function and static the delta does not name is in `now` what it is in `base`: the same
/// compiled function, the same definition.
fn unapplied(base: Option<&Built>, now: &Built) -> Option<String> {
    let base = base?;
    let delta = &now.delta;
    let mut keys: Vec<InstanceKey> = (base.inputs.iter().map(|it| it.0))
        .filter(|key| !delta.functions.removed.contains(key))
        .chain(delta.functions.added.iter().copied())
        .collect();
    keys.sort_unstable();
    let now_keys: Vec<InstanceKey> = now.inputs.iter().map(|it| it.0).collect();
    if keys != now_keys {
        return Some(format!(
            "the base's functions with the delta applied are {keys:?}, the build's {now_keys:?}"
        ));
    }
    let named = |key: &InstanceKey| {
        delta.functions.added.contains(key) || delta.functions.changed.contains(key)
    };
    for function in now.functions.iter().filter(|it| !named(&it.key)) {
        let was = base.functions.iter().find(|it| it.key == function.key);
        if was != Some(function) {
            return Some(format!(
                "the delta names no change of {:?}, whose function was {was:?} and is {function:?}",
                function.key
            ));
        }
    }
    let mut ids: Vec<StaticId> = (base.static_owners.iter().map(|it| (it.0, it.1)))
        .filter(|id| !delta.statics.removed.contains(id))
        .chain(delta.statics.added.iter().copied())
        .collect();
    ids.sort_unstable();
    let mut now_ids: Vec<StaticId> = now.static_owners.iter().map(|it| (it.0, it.1)).collect();
    now_ids.sort_unstable();
    if ids != now_ids {
        return Some(format!(
            "the base's statics with the delta applied are {ids:?}, the build's {now_ids:?}"
        ));
    }
    let named =
        |id: &StaticId| delta.statics.added.contains(id) || delta.statics.changed.contains(id);
    for value in now.statics.iter().filter(|it| !named(&(it.item, it.instance))) {
        let was = (base.statics.iter()).find(|it| (it.item, it.instance) == (value.item, value.instance));
        if was != Some(value) {
            return Some(format!(
                "the delta names no change of static {:?}, which was {was:?} and is {value:?}",
                (value.item, value.instance)
            ));
        }
    }
    None
}

/// What a delta from entries `before` to entries `now` must be, made another way than the
/// driver makes it: by key, through a map.
fn changes_between<K: Copy + Ord>(
    before: &[(K, ContentId)],
    now: &[(K, ContentId)],
) -> Changes<K> {
    let (before, now): (BTreeMap<K, ContentId>, BTreeMap<K, ContentId>) =
        (before.iter().copied().collect(), now.iter().copied().collect());
    Changes {
        added: now.keys().filter(|it| !before.contains_key(*it)).copied().collect(),
        changed: (now.iter())
            .filter(|(key, entry)| before.get(*key).is_some_and(|was| was != *entry))
            .map(|(key, _)| *key)
            .collect(),
        removed: before.keys().filter(|it| !now.contains_key(*it)).copied().collect(),
    }
}

/// The oracle of a build's delta: `now.delta` is relative to `base`, the last build of the
/// program that was kept, and names exactly the functions and statics whose entries (all
/// that a `Built` says of each) are added, other or gone, so `base` with the delta applied
/// is `now`. With no base it says why and names nothing.
fn delta_differs(base: Option<&Built>, now: &Built) -> Option<String> {
    let Some(base) = base else {
        let none = now.delta.base.is_none() && now.delta.fresh.is_some();
        let empty = now.delta.functions.is_empty() && now.delta.statics.is_empty();
        return (!(none && empty)).then(|| format!("a first build's delta is {:?}", now.delta));
    };
    let want = Delta {
        base: Some(base.id),
        functions: changes_between(&delta::function_entries(base), &delta::function_entries(now)),
        statics: changes_between(&delta::static_entries(base), &delta::static_entries(now)),
        fresh: None,
    };
    (now.delta != want).then(|| format!("the delta is {:?}, where it is {want:?}", now.delta))
}

/// A program of 3 to 8 functions, `main` (1) a member's, each calling the next and some of
/// those after it; the others members', another crate's generics a member instantiates,
/// dependencies', or of one code and calls with the function before (one key, two
/// describes); up to two
/// statics some function names, one maybe pointing to a function; a body per function for the
/// recursion limit to count; maybe a call through a `dyn`. And how it is built.
fn drawn(tc: &hegel::TestCase) -> (Program, Run) {
    use hegel::generators as gs;
    let count: u32 = tc.draw(gs::integers::<u32>().min_value(3).max_value(8));
    let among = || gs::integers::<u32>().min_value(1).max_value(count);
    let mut program = Program::default();
    for id in 1..=count {
        let mut calls: Vec<u32> = Vec::new();
        for callee in id + 1..=count {
            if callee == id + 1 || tc.draw(gs::integers::<u8>().max_value(2)) == 0 {
                calls.push(callee);
            }
        }
        program.function(id, 0x10 + id as u8, &calls);
        program.bodies.insert(id, (id % 3, false));
        if id == 1 {
            program.functions.get_mut(&1).expect("just made").symbol = Some("main".to_owned());
            continue;
        }
        let kind = tc.draw(gs::integers::<u8>().max_value(5));
        if kind == 3 && id > 2 {
            // One code with the function before it, and its calls: one key of two describes,
            // as two instances of a generic whose code reads their arguments alike.
            let before = program.functions.get_mut(&(id - 1)).expect("made before it");
            let of = before.code_of.unwrap_or(id - 1);
            before.code_of = Some(of);
            let (body, calls) = (before.body, before.calls.clone());
            let function = program.functions.get_mut(&id).expect("just made");
            (function.code_of, function.body, function.calls) = (Some(of), body, calls);
            continue;
        }
        let function = program.functions.get_mut(&id).expect("just made");
        match kind {
            0 => (function.foreign, function.over_editable) = (true, true),
            1 => function.foreign = true,
            2 => {
                function.origin = Origin::Dependency(Package::new("dep"));
                // A dependency's function reads library inputs alone.
                program.engine.durable.insert(id);
            }
            // A member's function whose inputs are one crate's sources: asked again only
            // when one of them is written.
            4 => {
                program.engine.groups.insert(id, u64::from(id % 2));
            }
            _ => {}
        }
    }
    let statics: u32 = tc.draw(gs::integers::<u32>().max_value(2));
    for id in 1..=statics {
        program.statics.insert(id, (id as u8, None));
        let names = tc.draw(among());
        program.functions.get_mut(&names).expect("drawn among them").statics.push(id);
        if tc.draw(gs::booleans()) {
            program.static_calls.insert(id, vec![tc.draw(among())]);
        }
    }
    if tc.draw(gs::booleans()) {
        program.dyn_sites.insert(tc.draw(among()), tc.draw(among()));
    }
    let run = Run {
        live: tc.draw(gs::booleans()),
        static_root: statics > 0 && tc.draw(gs::booleans()),
        check: tc.draw(gs::booleans()).then(|| tc.draw(among())),
    };
    (program, run)
}

/// The refinement: for any program and any sequence of edits (a body, a reference added or
/// removed, a function added or removed, a static's value, a static named or pointing
/// elsewhere, a dyn fact, the recursion limit, the build's roots), the build that patches the
/// walk and the lists the builds before it kept is, after every edit, the build of a walk
/// that keeps none: every list of the `Built` and its ID are the same, and the same errors
/// where it is refused; its delta names exactly what changed since the last build of the
/// same roots, and that build's lists with the delta applied are its own.
#[hegel::test(test_cases = 64)]
fn a_walk_over_kept_describes_builds_what_a_fresh_walk_builds(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let (program, run) = drawn(&tc);
    let count: usize = tc.draw(gs::integers::<usize>().max_value(8));
    let mut edits = Vec::with_capacity(count);
    for _ in 0..count {
        let edit = EDITS[tc.draw(gs::integers::<usize>().max_value(EDITS.len() - 1))];
        let picks = (tc.draw(gs::integers::<u32>()), tc.draw(gs::integers::<u32>()));
        edits.push((edit, picks));
    }
    if let Some(found) = first_difference(program, run, &edits, None) {
        panic!("{found}");
    }
}

const WHOLE: Run = Run { live: false, static_root: false, check: None };

/// The control of the refinement for a changed function: an edit of 4's body gives it a new
/// key, which its caller's places name. A driver that takes its kept describe without asking
/// the front end builds 4 under its old key, and the oracle says so; the driver that asks
/// builds what the fresh walk builds.
#[test]
fn the_oracle_catches_a_changed_function_whose_callers_keep_its_old_key() {
    // `Edit::Body` of the fourth function of five.
    let edits = [(Edit::Body, (3, 0))];
    assert_eq!(first_difference(program(), WHOLE, &edits, None), None);
    let found = first_difference(program(), WHOLE, &edits, Some(Plant::Unasked))
        .expect("the planted bug was not found");
    assert!(found.starts_with("build 1: "), "{found}");
}

/// The control of the refinement for a removed reference: `main` stops calling 3, which
/// nothing else calls, so 3 and what only it reaches leave the build. An engine that sees no
/// change of `main` leaves its kept describe naming 3, which the walk then still reaches; the
/// oracle says so.
#[test]
fn the_oracle_catches_a_removed_ref_whose_target_stays_reached() {
    // `Edit::RemoveRef` of the second call of the first function: `main`'s call of 3.
    let edits = [(Edit::RemoveRef, (0, 1))];
    assert_eq!(first_difference(program(), WHOLE, &edits, None), None);
    let found = first_difference(program(), WHOLE, &edits, Some(Plant::RemovedRef))
        .expect("the planted bug was not found");
    assert!(found.starts_with("build 1: "), "{found}");
}

/// The control of the refinement for the program's dyn facts: 2 calls through a `dyn` that
/// the facts make direct to 5, then to 3. An engine that sees no change of the fact leaves the
/// body built against the old one, calling 5; the oracle says so. The engine that sees it
/// describes 2 again, and nothing else.
#[test]
fn the_oracle_catches_a_dyn_fact_change_that_invalidates_nothing() {
    let with_site = || {
        let mut program = program();
        program.dyn_sites.insert(2, 5);
        program
    };
    // `Edit::DynFact`: the second function's call is direct to the third.
    let edits = [(Edit::DynFact, (1, 2))];
    assert_eq!(first_difference(with_site(), WHOLE, &edits, None), None);
    let found = first_difference(with_site(), WHOLE, &edits, Some(Plant::DynFact))
        .expect("the planted bug was not found");
    assert!(found.starts_with("build 1: "), "{found}");
}

/// A tree of `count` functions: `i` calls `2i` and `2i + 1`.
fn tree(count: u32) -> Program {
    let mut program = Program::default();
    for id in 1..=count {
        let calls: Vec<u32> = [2 * id, 2 * id + 1].into_iter().filter(|it| *it <= count).collect();
        program.function(id, 0x11, &calls);
    }
    program.functions.get_mut(&1).expect("the root").symbol = Some("main".to_owned());
    program
}

/// A walk's describes, kept describes taken, functions and functions dropped.
fn counts(walk: &Walk) -> (usize, usize, usize, usize) {
    (walk.described, walk.verified, walk.reached, walk.dropped)
}

fn describes(program: &Program) -> (usize, usize) {
    let engine = &program.engine;
    (engine.describes.load(Ordering::Relaxed), engine.verifies.load(Ordering::Relaxed))
}

/// The walk's cost in describes is the edit's, not the program's: of 300 functions, an edit
/// of one leaf's body describes that leaf alone, and a build after no edit describes nothing.
/// The first build describes them all and says why ([`Fresh::Restart`]).
#[test]
fn an_edit_of_one_leaf_describes_it_alone() {
    const COUNT: usize = 300;
    let mut program = tree(COUNT as u32);
    let kept = Kept::new();
    let walk = |program: &Program| {
        program.begin_build();
        let before = describes(program);
        let built = built(program, WHOLE, Some(&kept), None);
        let walk = built.unwrap_or_else(|e| panic!("{e:?}")).walk;
        let after = describes(program);
        (walk, (after.0 - before.0, after.1 - before.1))
    };
    let (first, asked) = walk(&program);
    assert_eq!(counts(&first), (COUNT, 0, COUNT, 0));
    assert_eq!((first.fresh, asked), (Some(Fresh::Restart), (COUNT, 0)));
    assert!(first.whole.is_some(), "nothing was kept to patch");
    assert_eq!(kept.len(), COUNT);

    program.functions.get_mut(&(COUNT as u32)).expect("the last leaf").body = 0x12;
    program.touch(COUNT as u32);
    let (edited, asked) = walk(&program);
    // The leaf has a new key: the one of the walk before is the one function dropped.
    assert_eq!(counts(&edited), (1, COUNT - 1, COUNT, 1));
    assert_eq!((edited.fresh, edited.whole, asked), (None, None, (1, COUNT - 1)));

    let (again, asked) = walk(&program);
    assert_eq!(counts(&again), (0, COUNT, COUNT, 0));
    assert_eq!((again.fresh, again.whole, asked), (None, None, (0, COUNT)));

    // A new function the leaf calls: the leaf, whose references changed, and the new one.
    program.function(COUNT as u32 + 1, 0x11, &[]);
    program.touch(COUNT as u32 + 1);
    program.functions.get_mut(&(COUNT as u32)).expect("the last leaf").calls.push(COUNT as u32 + 1);
    program.touch(COUNT as u32);
    let (grown, asked) = walk(&program);
    // The patch asked of every function, set the leaf's edges again and described the one
    // function that entered.
    assert_eq!(grown.whole, None);
    assert_eq!((grown.described, grown.verified, grown.reached), (2, COUNT - 1, COUNT + 1));
    assert_eq!(asked, (2, COUNT - 1));
}

/// A build given nothing to keep describes in describes everything and says so.
#[test]
fn a_build_that_keeps_no_walk_says_so() {
    let walk = built(&program(), WHOLE, None, None).unwrap_or_else(|e| panic!("{e:?}")).walk;
    assert_eq!(counts(&walk), (5, 0, 5, 0));
    assert_eq!((walk.fresh, walk.whole), (Some(Fresh::Unkept), Some("the build keeps no walk")));
}

/// The two rounds of a walk over a program with dyn facts keep their describes apart: the
/// next build takes each round's, and a changed fact describes again only the body that read
/// it, in the round against the facts.
#[test]
fn each_round_of_a_walk_keeps_its_own_describes() {
    let mut program = program();
    program.dyn_sites.insert(2, 5);
    let kept = Kept::new();
    let walk = |program: &Program| {
        program.begin_build();
        built(program, WHOLE, Some(&kept), None).unwrap_or_else(|e| panic!("{e:?}")).walk
    };
    let first = walk(&program);
    assert_eq!((first.described, first.verified, first.reached), (10, 0, 5));
    let again = walk(&program);
    assert_eq!((again.described, again.verified, again.reached), (0, 10, 5));
    set_fact(&mut program, 2, 3);
    let flipped = walk(&program);
    assert_eq!((flipped.described, flipped.verified, flipped.reached), (1, 9, 5));
}

/// A function's entry holds what the link reads of it beside its code: a leaf whose owner
/// alone changes keeps its key and its bytes, and the delta names it changed, alone.
#[test]
fn a_delta_names_a_function_whose_owner_alone_changed() {
    let mut program = program();
    let kept = Kept::new();
    program.begin_build();
    let before = built(&program, WHOLE, Some(&kept), None).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!((before.delta.base, before.delta.fresh), (None, Some(Fresh::Restart)));
    program.functions.get_mut(&5).expect("a leaf").owner = Owner::Dependency;
    program.touch(5);
    program.begin_build();
    let after = built(&program, WHOLE, Some(&kept), None).unwrap_or_else(|e| panic!("{e:?}"));
    let leaf = key_named(&after, 5);
    assert_eq!(key_named(&before, 5), leaf, "its owner is no part of its key");
    assert_eq!(after.delta.base, Some(before.id));
    assert_ne!(after.id, before.id);
    let changed = Changes { added: Vec::new(), changed: vec![leaf], removed: Vec::new() };
    assert_eq!(after.delta.functions, changed);
    assert!(after.delta.statics.is_empty());

    // A build after no edit changes nothing and is the same `Built`.
    program.begin_build();
    let again = built(&program, WHOLE, Some(&kept), None).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!((again.id, again.delta.base), (after.id, Some(after.id)));
    assert!(again.delta.functions.is_empty() && again.delta.statics.is_empty());
}

/// The control of the patched walk's shape rule: `main` stops calling 3. A driver that
/// patches a changed function whatever its shape gives `main` its new describe over its old
/// places, so the walk still holds 3 and what it reaches; the oracle says so. The driver that
/// holds the rule walks whole.
#[test]
fn the_oracle_catches_a_patch_that_keeps_a_changed_functions_old_references() {
    let edits = [(Edit::RemoveRef, (0, 1))];
    assert_eq!(first_difference(program(), WHOLE, &edits, None), None);
    let found = first_difference(program(), WHOLE, &edits, Some(Plant::AnyShape))
        .expect("the planted bug was not found");
    assert!(found.starts_with("build 1: "), "{found}");
}

/// The control of the patched walk's renaming: an edit of 4's body gives it a new key, which
/// 2's place must name. A driver that leaves the places that named the old key builds 2
/// calling a function the build no longer holds; the oracle says so.
#[test]
fn the_oracle_catches_a_caller_that_names_a_changed_callees_old_key() {
    let edits = [(Edit::Body, (3, 0))];
    assert_eq!(first_difference(program(), WHOLE, &edits, None), None);
    let found = first_difference(program(), WHOLE, &edits, Some(Plant::Unrenamed))
        .expect("the planted bug was not found");
    assert!(found.starts_with("build 1: "), "{found}");
}

/// The patched walk's work is the edit's: of 10,000 functions of which all but the last
/// three are durable, an edit of the last leaf's body asks the front end of those three,
/// describes one, and touches it and the one function that calls it. An edit that adds a
/// call to a function already reached is patched too.
#[test]
fn a_leaf_edit_among_ten_thousand_functions_touches_the_leaf_and_its_caller() {
    const COUNT: u32 = 10_000;
    let mut program = tree(COUNT);
    // Each under a name of its own, so none is merged with another of the same code and a
    // change of one is a change of it alone.
    for id in 2..=COUNT {
        program.functions.get_mut(&id).expect("of the tree").symbol = Some(format!("f{id}"));
    }
    program.engine.durable.extend(1..=COUNT - 3);
    let kept = Kept::new();
    let walk = |program: &Program| {
        program.begin_build();
        let before = describes(program);
        let built = built(program, WHOLE, Some(&kept), None);
        let built = built.unwrap_or_else(|e| panic!("{e:?}"));
        let after = describes(program);
        (built, (after.0 - before.0, after.1 - before.1))
    };
    let (first, _) = walk(&program);
    assert!(first.walk.whole.is_some());

    program.functions.get_mut(&COUNT).expect("the last leaf").body = 0x12;
    program.touch(COUNT);
    let (edited, asked) = walk(&program);
    assert_eq!(edited.walk.whole, None, "patched");
    assert_eq!(counts(&edited.walk), (1, 2, COUNT as usize, 1));
    assert_eq!(asked, (1, 2), "the front end described one function and verified two");
    assert_eq!(edited.walk.visited, 2, "the leaf and the function that calls it");
    let leaf = key_named(&edited, COUNT);
    let changes = &edited.delta.functions;
    assert_eq!((&changes.added, changes.removed.len()), (&vec![leaf], 1));
    // Its caller's places name another key: its entry is another, its key the same.
    assert_eq!(changes.changed, vec![key_named(&edited, COUNT / 2)]);

    program.functions.get_mut(&COUNT).expect("the last leaf").calls.push(COUNT - 1);
    program.touch(COUNT);
    let (grown, _) = walk(&program);
    assert_eq!(grown.walk.whole, None, "patched");
    assert_eq!(grown.walk.reached, COUNT as usize);
}

/// What one build did, by pass ([`Work`]), without what is linear by design: the rows copied
/// into the `Built`'s contiguous lists. The bytes hashed are held to a bound of their own,
/// as a keyed set's path grows with the logarithm of the set.
fn counted_work(built: &Built) -> Work {
    assert!(built.work.patched, "the lists were patched: {:?}", built.walk.whole);
    assert_eq!(built.work.copied, built.inputs.len() + built.static_owners.len());
    assert!(built.work.hashed <= 64 * 1024, "{} bytes hashed", built.work.hashed);
    Work { copied: 0, hashed: 0, ..built.work }
}

/// The work of three edits of a tree of `count` functions, each under a name of its own (so
/// dedupe merges none), all durable but the last three, which are asked each build, and
/// the last one's caller, which hangs on a group of inputs: a leaf's body; a call added from
/// the last leaf to a function already reached; the last leaf removed.
fn work_of_three_edits(count: u32) -> [Work; 3] {
    let mut program = tree(count);
    for id in 2..=count {
        program.functions.get_mut(&id).expect("of the tree").symbol = Some(format!("f{id}"));
    }
    let caller = count / 2;
    program.engine.durable.extend((1..=count - 3).filter(|id| *id != caller));
    program.engine.groups.insert(caller, 7);
    let kept = Kept::new();
    let build = |program: &Program| {
        program.begin_build();
        built(program, WHOLE, Some(&kept), None).unwrap_or_else(|e| panic!("{e:?}"))
    };
    assert!(build(&program).walk.whole.is_some());

    program.functions.get_mut(&count).expect("the last leaf").body = 0x12;
    program.touch(count);
    let body = build(&program);
    assert_eq!(body.delta.functions.added.len(), 1);

    program.functions.get_mut(&count).expect("the last leaf").calls.push(count - 1);
    program.touch(count);
    let edge = build(&program);
    assert_eq!(edge.walk.reached, count as usize);

    program.functions.remove(&count);
    program.functions.get_mut(&caller).expect("its caller").calls.retain(|it| *it != count);
    program.touch(caller);
    let removed = build(&program);
    assert_eq!(removed.walk.reached, count as usize - 1);
    // The leaf, and its caller's old key: the caller's code names what it calls.
    assert_eq!(removed.delta.functions.removed.len(), 2);
    assert_eq!(removed.delta.functions.added.len(), 1);
    [counted_work(&body), counted_work(&edge), counted_work(&removed)]
}

/// The invariant: the work of every pass of a build after an edit is the edit's, not the
/// program's. A leaf's body edit, a call added to a function already reached and a function
/// removed each count the same work in a program of 100 functions and in one of 10,000, pass
/// by pass, and each count is a constant and what the edit changed.
#[test]
fn the_work_of_an_edit_is_the_same_among_a_hundred_functions_and_ten_thousand() {
    let (small, large) = (work_of_three_edits(100), work_of_three_edits(10_000));
    assert_eq!(small, large, "the counts do not grow with the program");
    for (edit, work) in ["a body", "an added call", "a removed function"].iter().zip(&large) {
        // Asked: the three functions asked each build and the one of the group written.
        assert!(work.asked <= 4 && work.groups == 1, "{edit}: {work:?}");
        assert!(work.described <= 2 && work.graph <= 2 && work.claims == 0, "{edit}: {work:?}");
        // The changed function and the functions that refer to it or to what it refers to.
        for count in [work.classified, work.resolved, work.built_in, work.owners] {
            assert!(count <= 4, "{edit}: {work:?}");
        }
        for count in [work.lookups, work.versions, work.deduped, work.reach, work.replaced] {
            assert!(count <= 4, "{edit}: {work:?}");
        }
    }
    // A body edit: the leaf under its new key and its caller, whose place names it.
    assert_eq!((large[0].lookups, large[0].replaced), (2, 3));
    // A function removed: its row and its caller's old one go, and its caller under its
    // new key and the function that calls that one are made again.
    assert_eq!((large[2].lookups, large[2].replaced), (2, 4));
}

/// The control of [`the_work_of_an_edit_is_the_same_among_a_hundred_functions_and_ten_thousand`]:
/// a build that dedupes whole after an edit hashes every function, and the count says so. Two leaves of one code are one class; an edit of one's
/// body changes the class, so dedupe runs over all 100 functions as made.
#[test]
fn the_oracle_catches_a_pass_that_runs_whole() {
    let mut program = tree(100);
    // Every function but the two last leaves under a name of its own: those two are equal.
    for id in 2..=98 {
        program.functions.get_mut(&id).expect("of the tree").symbol = Some(format!("f{id}"));
    }
    let kept = Kept::new();
    let build = |program: &Program| {
        program.begin_build();
        built(program, WHOLE, Some(&kept), None).unwrap_or_else(|e| panic!("{e:?}"))
    };
    let first = build(&program);
    assert_eq!(first.deduped.merged.len(), 1, "the two leaves are one function");
    program.functions.get_mut(&100).expect("the last leaf").body = 0x12;
    program.touch(100);
    let edited = build(&program);
    assert!(edited.work.patched);
    assert!(edited.deduped.merged.is_empty());
    assert_eq!(edited.work.deduped, 100, "dedupe ran over every function");
}

/// Builds `program`, changes it by `edit`, and builds again over what the first build kept
/// with `plant`: the first difference from a fresh build of the changed program.
fn difference_after(
    mut program: Program,
    edit: impl Fn(&mut Program),
    plant: Option<Plant>,
) -> Option<String> {
    let kept = Kept::new();
    program.begin_build();
    built(&program, WHOLE, Some(&kept), None).unwrap_or_else(|e| panic!("{e:?}"));
    edit(&mut program);
    program.begin_build();
    let over_kept = built(&program, WHOLE, Some(&kept), plant);
    difference(&over_kept, &built(&program.copy(), WHOLE, None, None))
}

/// The control of the lists' rows: an edit of 4's body gives it a new key, which the
/// function made of 2's template must name. A build that keeps the function 2 was made of
/// gives the linker a call of a function it no longer holds; the oracle says so.
#[test]
fn the_oracle_catches_a_kept_template_used_after_its_callees_key_changed() {
    let edits = [(Edit::Body, (3, 0))];
    let plant = Some(Plant::Patch(Planted::KeptTemplates));
    assert_eq!(first_difference(program(), WHOLE, &edits, None), None);
    let found = first_difference(program(), WHOLE, &edits, plant)
        .expect("the planted bug was not found");
    assert!(found.starts_with("build 1: "), "{found}");
}

/// The control of the delta: an edit of 4's body takes its old key out of the build. A delta
/// that names no removed function leaves it in the state of whoever applies it; the oracle
/// says so.
#[test]
fn the_oracle_catches_a_delta_that_omits_a_removed_function() {
    let edits = [(Edit::Body, (3, 0))];
    let plant = Some(Plant::Patch(Planted::DeltaOmitsRemoved));
    let found = first_difference(program(), WHOLE, &edits, plant)
        .expect("the planted bug was not found");
    assert!(found.starts_with("build 1: the delta is"), "{found}");
}

/// The control of a row's owner: a leaf's owner changes and nothing else of it. A build that
/// keeps the owner its row had gives the dependency image's split the old one; the oracle
/// says so.
#[test]
fn the_oracle_catches_an_owner_kept_when_its_function_changed() {
    let edit = |program: &mut Program| {
        program.functions.get_mut(&5).expect("a leaf").owner = Owner::Dependency;
        program.touch(5);
    };
    assert_eq!(difference_after(program(), edit, None), None);
    let found = difference_after(program(), edit, Some(Plant::Patch(Planted::KeptOwner)))
        .expect("the planted bug was not found");
    assert!(found.starts_with("`owners` differs"), "{found}");
}

/// A program whose two leaves are one code, so dedupe merges them, and then their callers.
fn merged() -> Program {
    let mut program = program();
    program.functions.get_mut(&5).expect("a leaf").body = 0x44;
    program
}

/// The control of dedupe's classes: 4 and 5 are one function until 5's body is edited. A
/// build that leaves the classes as they were when a member's code changed still says 5 was
/// merged; the oracle says so.
#[test]
fn the_oracle_catches_a_dedupe_class_not_split_when_a_members_code_changed() {
    let edits = [(Edit::Body, (4, 0))];
    let plant = Some(Plant::Patch(Planted::UnsplitClass));
    assert_eq!(first_difference(merged(), WHOLE, &edits, None), None);
    let found =
        first_difference(merged(), WHOLE, &edits, plant).expect("the planted bug was not found");
    assert!(found.starts_with("build 1: "), "{found}");
}

/// The control of the recursion limit: 2, 4 and 5 are instances of one body, under a limit of
/// one repeat. `main` reaches 4 through 2 and 5 through 3, each within the limit; a call
/// from 4 to 5 claims 5 on the path through 2 and 4, past it. A patch that runs no check
/// after the added edge builds what the whole walk refuses; the oracle says so.
#[test]
fn the_oracle_catches_a_recursion_claim_not_run_after_an_added_edge() {
    let limited = || {
        let mut program = program();
        for id in [2, 4, 5] {
            program.bodies.insert(id, (7, false));
        }
        program.recursion_limit = Some(1);
        program
    };
    // `Edit::AddRef`: the fourth function calls the fifth.
    let edits = [(Edit::AddRef, (3, 4))];
    let plant = Some(Plant::Patch(Planted::SkippedClaims));
    assert_eq!(first_difference(limited(), WHOLE, &edits, None), None);
    let found =
        first_difference(limited(), WHOLE, &edits, plant).expect("the planted bug was not found");
    assert!(found.starts_with("build 1: built over kept describes, refused fresh"), "{found}");
}

/// A function added and a function removed are patched: the walk describes the one that
/// entered and drops the one nothing reaches, and the delta names each.
#[test]
fn an_added_and_a_removed_function_are_patched() {
    let mut program = program();
    let kept = Kept::new();
    let build = |program: &Program| {
        program.begin_build();
        built(program, WHOLE, Some(&kept), None).unwrap_or_else(|e| panic!("{e:?}"))
    };
    let first = build(&program);
    program.function(6, 0x66, &[]);
    program.touch(6);
    program.functions.get_mut(&4).expect("a leaf").calls.push(6);
    program.touch(4);
    let grown = build(&program);
    assert_eq!((grown.walk.whole, grown.walk.reached), (None, 6));
    assert!(grown.delta.functions.added.contains(&key_named(&grown, 6)));
    assert_eq!(grown.delta.base, Some(first.id));
    // `main` stops calling 3: 3 and 5, which nothing else reaches, leave.
    program.functions.get_mut(&1).expect("main").calls.retain(|it| *it != 3);
    program.touch(1);
    let shrunk = build(&program);
    assert_eq!((shrunk.walk.whole, shrunk.walk.reached), (None, 4));
    let gone = [key_named(&grown, 3), key_named(&grown, 5)];
    assert!(gone.iter().all(|key| shrunk.delta.functions.removed.contains(key)));
    assert_eq!(shrunk.walk.dropped, shrunk.delta.functions.removed.len());
}
