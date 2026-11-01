//! Code dedupe (docs/v2/plan/lanes/items.md section 6.4): functions of a build that run the same
//! instructions against the same addresses are one function.
//!
//! Functions are grouped by [`code_id`]; each group's least key is kept and callers'
//! relocations are pointed at it. That can make callers equal in turn, so it repeats until
//! nothing changes, as a linker's identical code folding does. What may be merged is the
//! caller's to say ([`dedupe`]'s `mergeable`): never a function whose address is significant
//! (an exported symbol), never one reached through its slot (a member of a live build, which
//! an edit patches alone). Statics are never functions here: [`crate::Target::Static`] is by
//! which item, and its value is never merged.

use std::collections::BTreeMap;

use blaze_hash::HashMap;
use blaze_ids::ContentId;

use crate::{CompiledFunction, Data, InstanceKey, Relocs, Target, Unwind, content::code_id};

/// What became of a build's functions.
#[derive(Clone, PartialEq, Eq, Debug, Default)] // clone: inside `driver::Built`, once per build
pub struct Deduped {
    /// Each function merged away, with the one kept for it.
    // BTreeMap: a build's output, sorted once when it is made.
    pub merged: BTreeMap<InstanceKey, InstanceKey>,
}

fn renamed(key: &InstanceKey, kept: &HashMap<InstanceKey, InstanceKey>) -> InstanceKey {
    kept.get(key).copied().unwrap_or(*key)
}

fn rename_target(
    target: &Target,
    kept: &HashMap<InstanceKey, InstanceKey>,
    data: &HashMap<ContentId, ContentId>,
) -> Target {
    match target {
        Target::Function(key) => Target::Function(renamed(key, kept)),
        Target::Direct(key) => Target::Direct(renamed(key, kept)),
        Target::Data(id) => Target::Data(data.get(id).copied().unwrap_or(*id)),
        // Made only for a function that calls one merged away; `Relocs::map` borrows the old.
        // clone: a renamed function's new relocation table
        other => other.clone(),
    }
}

fn rename_relocs(
    relocs: &Relocs,
    kept: &HashMap<InstanceKey, InstanceKey>,
    data: &HashMap<ContentId, ContentId>,
) -> Relocs {
    relocs.map(|target| rename_target(target, kept, data))
}

fn rename_unwind(
    unwind: Unwind,
    kept: &HashMap<InstanceKey, InstanceKey>,
    data: &HashMap<ContentId, ContentId>,
) -> Unwind {
    let routine = |it: &Option<Target>| it.as_ref().map(|it| rename_target(it, kept, data));
    let area = |it: Option<ContentId>| it.map(|id| data.get(&id).copied().unwrap_or(id));
    match unwind {
        Unwind::Dwarf { code_align, data_align, return_address, cie, fde, personality, lsda } => Unwind::Dwarf {
            code_align,
            data_align,
            return_address,
            cie,
            fde,
            personality: routine(&personality),
            lsda: area(lsda),
        },
        Unwind::Windows { info, handler, lsda } => {
            Unwind::Windows { info, handler: routine(&handler), lsda: area(lsda) }
        }
    }
}

/// Renames what `function` calls by `kept`, in place, and makes its data's IDs again; leaves
/// it as it is when it names nothing of `kept`.
fn rename(function: &mut CompiledFunction, kept: &HashMap<InstanceKey, InstanceKey>) {
    let names = |target: &Target| match target {
        Target::Function(key) | Target::Direct(key) => kept.contains_key(key),
        _ => false,
    };
    let names_one = |relocs: &Relocs| relocs.targets().iter().any(names);
    let routine_named = match &function.unwind {
        Some(Unwind::Dwarf { personality: Some(it), .. } | Unwind::Windows { handler: Some(it), .. }) => names(it),
        _ => false,
    };
    if !names_one(&function.relocs) && !function.data.iter().any(|it| names_one(&it.relocs)) && !routine_named {
        return;
    }
    let mut ids = HashMap::default();
    for it in &mut function.data {
        let relocs = rename_relocs(&it.relocs, kept, &ids);
        let made = Data::new(it.align, std::mem::take(&mut it.bytes), relocs);
        ids.insert(it.id, made.id);
        *it = made;
    }
    function.relocs = rename_relocs(&function.relocs, kept, &ids);
    function.unwind = function.unwind.take().map(|it| rename_unwind(it, kept, &ids));
}

/// Merges the functions of a build whose code is equal, in place: `functions` keeps one of
/// each group, and every relocation that named one merged away names the one kept.
/// `mergeable` says which may be merged; any function's callers are renamed.
pub fn dedupe(
    functions: &mut Vec<CompiledFunction>,
    mergeable: impl Fn(&InstanceKey) -> bool,
) -> Deduped {
    // Unordered: `merged` is sorted as it is made.
    let mut kept: HashMap<InstanceKey, InstanceKey> = HashMap::default();
    loop {
        // Unordered: each group is sorted on its own, and only fills `more`, a map by key.
        let mut groups: HashMap<ContentId, Vec<InstanceKey>> = HashMap::default();
        // A covered function counts its own blocks, a counting one its own calls and an
        // instrumented one its own PGO record: merged, two would share counters.
        let counts = |it: &CompiledFunction| {
            it.profile.is_some_and(|it| it.counted().is_some())
                || it.relocs.targets().iter().any(|it| matches!(it, Target::Counter(_) | Target::ProfileCounters))
        };
        for function in functions.iter().filter(|it| mergeable(&it.key) && it.coverage.is_none() && !counts(it)) {
            groups.entry(code_id(function)).or_default().push(function.key);
        }
        let mut more: HashMap<InstanceKey, InstanceKey> = HashMap::default();
        for mut keys in groups.into_values().filter(|it| it.len() > 1) {
            keys.sort();
            let first = keys[0];
            more.extend(keys[1..].iter().map(|key| (*key, first)));
        }
        if more.is_empty() {
            break;
        }
        functions.retain(|it| !more.contains_key(&it.key));
        for to in kept.values_mut() {
            *to = renamed(to, &more);
        }
        kept.extend(more.iter().map(|(from, to)| (*from, *to)));
        for function in functions.iter_mut() {
            rename(function, &more);
        }
    }
    functions.sort_by_key(|it| it.key);
    // alloc: the build's answer, sorted once as it is made
    Deduped { merged: kept.into_iter().collect() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Arch, RelocKind, Relocation};

    fn key(n: u8) -> InstanceKey {
        InstanceKey(ContentId([n; 32]))
    }

    fn function(n: u8, code: u8, calls: &[u8]) -> CompiledFunction {
        let relocs = calls
            .iter()
            .enumerate()
            .map(|(at, callee)| Relocation {
                at: 1 + 5 * at as u32,
                kind: RelocKind::PcRel32,
                target: Target::Direct(key(*callee)),
                addend: -4,
            })
            .collect();
        CompiledFunction {
            key: key(n),
            arch: Arch::X86_64,
            align: 16,
            code: vec![code; 1 + 5 * calls.len()],
            relocs,
            data: Vec::new(),
            inlined: Vec::new(),
            side: None,
            unwind: None,
            lines: Vec::new(),
            coverage: None,
            profile: None,
        }
    }

    #[test]
    fn an_lsda_follows_its_data_and_a_personality_its_function() {
        // 2 and 3 are equal; 9's personality is 3 and its LSDA points at 3.
        let lsda = |to: u8| {
            let relocs = vec![Relocation { at: 0, kind: RelocKind::Abs64, target: Target::Direct(key(to)), addend: 0 }];
            Data::new(4, vec![0; 4], relocs.into())
        };
        let mut caller = function(9, 0x55, &[]);
        let area = lsda(3);
        caller.unwind = Some(Unwind::Dwarf {
            code_align: 1,
            data_align: -8,
            return_address: 16,
            cie: Vec::new(),
            fde: Vec::new(),
            personality: Some(Target::Direct(key(3))),
            lsda: Some(area.id),
        });
        caller.data.push(area);
        let mut functions = vec![function(2, 0x90, &[]), function(3, 0x90, &[]), caller];
        dedupe(&mut functions, |it| *it != key(9));
        let caller = functions.iter().find(|it| it.key == key(9)).unwrap();
        assert!(matches!(
            &caller.unwind,
            Some(Unwind::Dwarf { personality: Some(Target::Direct(it)), lsda: Some(id), .. })
                if *it == key(2) && *id == lsda(2).id
        ));
    }

    #[test]
    fn equal_code_is_one_function_and_callers_follow() {
        // 2 and 3 are equal; 4 calls 2 and 5 calls 3, so they become equal too.
        let mut functions = vec![
            function(2, 0x90, &[]),
            function(3, 0x90, &[]),
            function(4, 0xcc, &[2]),
            function(5, 0xcc, &[3]),
            function(6, 0xc3, &[5]),
        ];
        let deduped = dedupe(&mut functions, |_| true);
        assert_eq!(deduped.merged, BTreeMap::from([(key(3), key(2)), (key(5), key(4))]));
        let keys: Vec<_> = functions.iter().map(|it| it.key).collect();
        assert_eq!(keys, vec![key(2), key(4), key(6)]);
        assert_eq!(functions[2].relocs.targets(), [Target::Direct(key(4))]);
    }

    #[test]
    fn covered_functions_count_apart() {
        let covered = |n| CompiledFunction {
            coverage: Some(crate::Covered { counters: 1, flags: 0, guards: 0, pcs: vec![(0, 1)] }),
            ..function(n, 0x90, &[])
        };
        let mut functions = vec![covered(2), covered(3)];
        assert!(dedupe(&mut functions, |_| true).merged.is_empty());
        assert_eq!(functions.len(), 2);
    }

    #[test]
    fn counting_functions_count_apart() {
        let counting = |n| {
            let mut it = function(n, 0x90, &[]);
            it.relocs.push(Relocation { at: 0, kind: RelocKind::PcRel32, target: Target::Counter(key(n)), addend: 4 });
            it
        };
        let mut functions = vec![counting(2), counting(3), function(7, 0xc3, &[3])];
        assert!(dedupe(&mut functions, |_| true).merged.is_empty());
        assert_eq!(functions.len(), 3);
        assert_eq!(functions[0].relocs.targets(), [Target::Counter(key(2))]);
    }

    /// Instrumented functions keep their own records and counters; ones built with a profile
    /// merge as any code, a mismatched one only with another mismatched.
    #[test]
    fn instrumented_functions_count_apart() {
        let record = crate::ProfileRecord { name: [1; 16], name_ref: 2, hash: 3, counters: 1, version: 10 };
        let profiled = |n, profile| CompiledFunction { profile: Some(profile), ..function(n, 0x90, &[]) };
        let mut functions = vec![profiled(2, crate::Profiled::Counted(record)), profiled(3, crate::Profiled::Counted(record))];
        functions[0].relocs.push(Relocation { at: 0, kind: RelocKind::PcRel32, target: Target::ProfileCounters, addend: 4 });
        functions[1].relocs.push(Relocation { at: 0, kind: RelocKind::PcRel32, target: Target::ProfileCounters, addend: 4 });
        assert!(dedupe(&mut functions, |_| true).merged.is_empty());
        assert_eq!(functions.len(), 2);

        let used = |mismatched| crate::Profiled::Used { mismatched };
        let mut functions = vec![profiled(2, used(true)), profiled(3, used(true)), profiled(4, used(false))];
        let deduped = dedupe(&mut functions, |_| true);
        assert_eq!(deduped.merged, BTreeMap::from([(key(3), key(2))]));
        assert_eq!(functions.len(), 2);
    }

    #[test]
    fn what_may_not_be_merged_stays_but_its_callers_are_renamed() {
        let mut functions =
            vec![function(2, 0x90, &[]), function(3, 0x90, &[]), function(7, 0xc3, &[3])];
        let deduped = dedupe(&mut functions, |it| *it != key(2));
        assert!(deduped.merged.is_empty());
        assert_eq!(functions.len(), 3);
        let mut functions =
            vec![function(2, 0x90, &[]), function(3, 0x90, &[]), function(7, 0xc3, &[3])];
        dedupe(&mut functions, |it| *it != key(7));
        assert_eq!(functions.iter().find(|it| it.key == key(7)).unwrap().relocs.targets(), [Target::Direct(key(2))]);
    }

    #[test]
    fn data_pointing_at_a_merged_function_gets_its_new_id() {
        let vtable = |to: u8| {
            Data::new(
                8,
                vec![0; 8],
                vec![Relocation { at: 0, kind: RelocKind::Abs64, target: Target::Direct(key(to)), addend: 0 }].into(),
            )
        };
        let mut caller = function(9, 0x55, &[]);
        let table = vtable(3);
        caller.relocs.push(Relocation { at: 0, kind: RelocKind::PcRel32, target: Target::Data(table.id), addend: -4 });
        caller.data.push(table);
        let mut functions = vec![function(2, 0x90, &[]), function(3, 0x90, &[]), caller];
        dedupe(&mut functions, |_| true);
        let caller = functions.iter().find(|it| it.key == key(9)).unwrap();
        assert_eq!(caller.data[0], vtable(2));
        assert_eq!(caller.relocs.targets(), [Target::Data(vtable(2).id)]);
    }

    #[test]
    fn the_result_is_the_same_whatever_the_order() {
        let make = || {
            vec![
                function(2, 0x90, &[]),
                function(3, 0x90, &[]),
                function(4, 0xcc, &[2]),
                function(5, 0xcc, &[3]),
            ]
        };
        let mut a = make();
        let mut b = make();
        b.reverse();
        assert_eq!(dedupe(&mut a, |_| true), dedupe(&mut b, |_| true));
        assert_eq!(a, b);
    }

    #[test]
    fn many_groups_merge_the_same_whatever_the_order() {
        // 60 functions of 8 codes, each calling one of the first 20: groups merge over rounds.
        let make = || -> Vec<CompiledFunction> {
            (1..=60u8)
                .map(|n| match n {
                    1..=20 => function(n, n % 4, &[]),
                    _ => function(n, 0x40 + n % 4, &[1 + n % 20]),
                })
                .collect()
        };
        let mut first = make();
        let want = dedupe(&mut first, |it| *it != key(7));
        assert!(want.merged.len() > 20, "{}", want.merged.len());
        for step in [7usize, 11, 13, 59] {
            let mut functions = make();
            let len = functions.len();
            let permuted: Vec<CompiledFunction> =
                (0..len).map(|at| functions[at * step % len].clone()).collect();
            functions = permuted;
            assert_eq!(dedupe(&mut functions, |it| *it != key(7)), want);
            assert_eq!(functions, first);
        }
    }
}
