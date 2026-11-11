//! Code dedupe (docs/v2/plan/design/item-hash.md section 6.4, docs/v2/plan/design/icf.md):
//! functions of a build that run the same instructions against the same addresses are one
//! function.
//!
//! Functions are grouped by [`code_id`], which takes each relocation by what it names (an
//! instance's key, a datum's content ID, a static's item), never by an address; each group's
//! least key is kept and every relocation that named one merged away names the one kept: in
//! code, in data, in unwind tables and in statics' values. That can make callers equal in
//! turn, so it repeats until nothing changes, as a linker's identical code folding does, and
//! ends within one round per function it may merge ([`DedupeError::NoFixpoint`] otherwise).
//! What may be merged is the caller's to say ([`dedupe`]'s `mergeable`): never a function
//! whose address is significant (an exported symbol), never one reached through its slot (a
//! member of a live build, which an edit patches alone). Statics are never functions here:
//! [`crate::Target::Static`] is by which item, and its value is never merged.
//!
//! Rust lets two functions share an address: rustc declares every Rust function
//! `unnamed_addr` ("Function addresses in Rust are never significant, allowing functions to be
//! merged", `rustc_codegen_llvm::declare::declare_fn`), and `std`'s `fn` docs say "pointers to
//! different functions can compare equal (since identical functions can be deduplicated
//! within a codegen unit)".

use std::alloc::Global;
use std::collections::BTreeMap;
use std::fmt;

use blaze_hash::HashMap;
use blaze_ids::{ContentId, Hasher};
use blaze_items::ItemKey;

use crate::{
    CompiledFunction, Data, Init, InstanceKey, Relocs, Static, Target, Unwind, content::code_id,
    content::data_id,
};

/// The domain of a writable datum's ID once dedupe renamed what it points at: its old ID (its
/// static's, by [`Data::owned`]) and its new content, so it stays its static's own object and
/// is another object once what it points at changes.
const RENAMED_OWNED_DOMAIN: &str = "blaze v2 renamed owned data 1";

/// What became of a build's functions.
#[derive(Clone, PartialEq, Eq, Debug, Default)] // clone: inside `driver::Built`, once per build
pub struct Deduped {
    /// Each function merged away, with the one kept for it.
    // BTreeMap: a build's output, sorted once when it is made.
    pub merged: BTreeMap<InstanceKey, InstanceKey>,
    /// Rounds the fixpoint took, the last of which merged nothing.
    pub rounds: u32,
    /// Bytes of code the functions merged away had: what the image no longer holds.
    pub bytes: u64,
}

/// Whose data a [`DedupeError::DataOrder`] is in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DataOwner {
    Function(InstanceKey),
    Static(ItemKey, ContentId),
}

/// Why dedupe stopped: each a bug of dedupe or of the code it was given, never a build's to
/// work around.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DedupeError {
    /// The fixpoint did not end within its bound: every round but the last merges at least
    /// one of `candidates`, so `candidates + 1` rounds always suffice. `merging` is a pair the
    /// last round merged, the function merged away and the one kept.
    NoFixpoint { rounds: u32, candidates: usize, merging: (InstanceKey, InstanceKey) },
    /// A datum names itself or a datum after it, which `Template::of` refuses: renaming in
    /// one pass needs each datum to name only those before it.
    DataOrder { owner: DataOwner, datum: ContentId, points_at: ContentId },
}

impl fmt::Display for DedupeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DedupeError::NoFixpoint { rounds, candidates, merging: (from, to) } => write!(
                f,
                "code dedupe found no fixpoint in {rounds} rounds over {candidates} functions (the bound is one round \
                 per function and one more); its last round still merged {from:?} into {to:?}"
            ),
            DedupeError::DataOrder { owner, datum, points_at } => write!(
                f,
                "code dedupe renamed the data of {owner:?}, whose datum {datum:?} names {points_at:?}, itself or a \
                 datum after it: a datum names only those before it (`Template::of`, `TemplateError::DataOrder`)"
            ),
        }
    }
}

impl DedupeError {
    /// The instance the error is about: the one being merged, or the function whose data it
    /// is; a static's is the default key, its item being [`DedupeError::item`].
    pub fn unit(&self) -> InstanceKey {
        match self {
            DedupeError::NoFixpoint { merging, .. } => merging.0,
            DedupeError::DataOrder { owner: DataOwner::Function(key), .. } => *key,
            DedupeError::DataOrder { owner: DataOwner::Static(..), .. } => {
                InstanceKey(ContentId::default())
            }
        }
    }

    pub fn item(&self) -> ItemKey {
        match self {
            DedupeError::DataOrder { owner: DataOwner::Static(item, _), .. } => *item,
            _ => ItemKey(ContentId::default()),
        }
    }
}

fn renamed(key: &InstanceKey, kept: &HashMap<InstanceKey, InstanceKey, Global>) -> InstanceKey {
    kept.get(key).copied().unwrap_or(*key)
}

fn rename_target(
    target: &Target,
    kept: &HashMap<InstanceKey, InstanceKey, Global>,
    data: &HashMap<ContentId, ContentId, Global>,
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
    kept: &HashMap<InstanceKey, InstanceKey, Global>,
    data: &HashMap<ContentId, ContentId, Global>,
) -> Relocs {
    relocs.map(|target| rename_target(target, kept, data))
}

fn rename_unwind(
    unwind: Unwind,
    kept: &HashMap<InstanceKey, InstanceKey, Global>,
    data: &HashMap<ContentId, ContentId, Global>,
) -> Unwind {
    let routine = |it: &Option<Target>| it.as_ref().map(|it| rename_target(it, kept, data));
    let area = |it: Option<ContentId>| it.map(|id| data.get(&id).copied().unwrap_or(id));
    match unwind {
        Unwind::Dwarf { code_align, data_align, return_address, cie, fde, personality, lsda } => {
            Unwind::Dwarf {
                code_align,
                data_align,
                return_address,
                cie,
                fde,
                personality: routine(&personality),
                lsda: area(lsda),
            }
        }
        Unwind::Windows { info, handler, lsda } => {
            Unwind::Windows { info, handler: routine(&handler), lsda: area(lsda) }
        }
    }
}

/// Whether `target` is a function of `kept`.
fn names(target: &Target, kept: &HashMap<InstanceKey, InstanceKey, Global>) -> bool {
    matches!(target, Target::Function(key) | Target::Direct(key) if kept.contains_key(key))
}

/// Whether `relocs` name a function of `kept`.
fn names_kept(relocs: &Relocs, kept: &HashMap<InstanceKey, InstanceKey, Global>) -> bool {
    relocs.targets().iter().any(|target| names(target, kept))
}

/// Renames `data`'s relocations by `kept` and gives each datum whose relocations changed its
/// ID again, in place: read-only data by its content ([`Data::new`]), a writable datum by its
/// old ID and its new content, so it stays its static's own. One forward pass suffices: a
/// datum only names data before it (`Template::of` refuses others, `TemplateError::DataOrder`),
/// so each datum it names already has its new ID; one that names itself or a later datum is
/// [`DedupeError::DataOrder`]. The answer is each changed datum's old ID with its new one.
fn rename_data(
    data: &mut [Data],
    kept: &HashMap<InstanceKey, InstanceKey, Global>,
    owner: DataOwner,
) -> Result<HashMap<ContentId, ContentId, Global>, DedupeError> {
    let mut ids: HashMap<ContentId, ContentId, Global> = HashMap::new_in(Global);
    for at in 0..data.len() {
        let it = &data[at];
        let later = |target: &Target| matches!(target, Target::Data(id) if data[at..].iter().any(|it| it.id == *id));
        if let Some(Target::Data(id)) = it.relocs.targets().iter().find(|it| later(it)) {
            return Err(DedupeError::DataOrder { owner, datum: it.id, points_at: *id });
        }
        let relocs = rename_relocs(&it.relocs, kept, &ids);
        if relocs == it.relocs {
            continue;
        }
        let it = &mut data[at];
        let content = data_id(it.align, &it.bytes, &relocs);
        let id = match it.writable {
            false => content,
            true => {
                let mut hasher = Hasher::new(RENAMED_OWNED_DOMAIN);
                hasher.id(&it.id).id(&content);
                hasher.finish()
            }
        };
        ids.insert(it.id, id);
        it.id = id;
        it.relocs = relocs;
    }
    Ok(ids)
}

/// Whether `function` names a function of `kept`: in its code, its data or its unwind
/// routine. What [`rename`] changes it for.
fn names_any(function: &CompiledFunction, kept: &HashMap<InstanceKey, InstanceKey, Global>) -> bool {
    let routine_named = match &function.unwind {
        Some(
            Unwind::Dwarf { personality: Some(it), .. } | Unwind::Windows { handler: Some(it), .. },
        ) => names(it, kept),
        _ => false,
    };
    names_kept(&function.relocs, kept)
        || function.data.iter().any(|it| names_kept(&it.relocs, kept))
        || routine_named
}

/// Whether `value` names a function of `kept`: what [`rename_static`] changes it for.
fn static_names_any(value: &Static, kept: &HashMap<InstanceKey, InstanceKey, Global>) -> bool {
    matches!(&value.init, Init::Bytes { relocs, .. } if names_kept(relocs, kept))
        || value.data.iter().any(|it| names_kept(&it.relocs, kept))
}

/// Renames what `function` calls by `kept`, in place, and makes its data's IDs again; leaves
/// it as it is, and says so, when it names nothing of `kept`.
pub(crate) fn rename(
    function: &mut CompiledFunction,
    kept: &HashMap<InstanceKey, InstanceKey, Global>,
) -> Result<bool, DedupeError> {
    if !names_any(function, kept) {
        return Ok(false);
    }
    let ids = rename_data(&mut function.data, kept, DataOwner::Function(function.key))?;
    function.relocs = rename_relocs(&function.relocs, kept, &ids);
    function.unwind = function.unwind.take().map(|it| rename_unwind(it, kept, &ids));
    Ok(true)
}

/// Renames what `value` points at by `kept`, in place, and makes its data's IDs again; says
/// whether anything changed, which changes its digest.
pub(crate) fn rename_static(
    value: &mut Static,
    kept: &HashMap<InstanceKey, InstanceKey, Global>,
) -> Result<bool, DedupeError> {
    if !static_names_any(value, kept) {
        return Ok(false);
    }
    let ids = rename_data(&mut value.data, kept, DataOwner::Static(value.item, value.instance))?;
    if let Init::Bytes { relocs, .. } = &mut value.init {
        *relocs = rename_relocs(relocs, kept, &ids);
    }
    Ok(true)
}

/// Whether dedupe may merge `function` with another of equal code, where its caller says its
/// key is `mergeable`. A covered function counts its own blocks, a counting one its own calls
/// and an instrumented one its own PGO record: merged, two would share counters.
pub(crate) fn candidate(function: &CompiledFunction, mergeable: bool) -> bool {
    let counts = function.profile.is_some_and(|it| it.counted().is_some())
        || (function.relocs.targets().iter())
            .any(|it| matches!(it, Target::Counter(_) | Target::ProfileCounters));
    mergeable && function.coverage.is_none() && !counts
}

/// What [`dedupe_keeping`] changed, as it was before: each function merged away or renamed
/// and each static renamed, as given. A build that keeps its functions across edits
/// (`driver::lists`) holds every function as it was made, so a later dedupe starts from
/// what this one started from.
#[derive(Default)]
pub struct Originals {
    pub functions: Vec<CompiledFunction>,
    pub statics: Vec<Static>,
}

/// Merges the functions of a build whose code is equal, in place: `functions` keeps one of
/// each group, and every relocation that named one merged away names the one kept, in the
/// functions and in `statics`. `mergeable` says which may be merged; any function's callers
/// are renamed. The answer says what was merged and the indices into `statics` of those
/// renamed, whose digests change.
pub fn dedupe(
    functions: &mut Vec<CompiledFunction>,
    statics: &mut [Static],
    mergeable: impl Fn(&InstanceKey) -> bool,
) -> Result<(Deduped, Vec<usize>), DedupeError> {
    dedupe_keeping(functions, statics, mergeable, None)
}

/// [`dedupe`], which also gives `originals`, when asked, each function and static it changes
/// or takes out as it was given ([`Originals`]): a copy of each renamed one, made before its
/// first rename, and each merged one itself.
pub fn dedupe_keeping(
    functions: &mut Vec<CompiledFunction>,
    statics: &mut [Static],
    mergeable: impl Fn(&InstanceKey) -> bool,
    mut originals: Option<&mut Originals>,
) -> Result<(Deduped, Vec<usize>), DedupeError> {
    let candidate = |it: &CompiledFunction| candidate(it, mergeable(&it.key));
    // The keys whose original is kept already: a function renamed in two rounds is copied
    // before the first.
    let mut copied: blaze_hash::HashSet<InstanceKey, Global> = blaze_hash::HashSet::new_in(Global);
    // Each candidate's code ID, by key: hashed once, and again only when a round renames it.
    // Unordered: only grouped, and each group is sorted on its own.
    let mut ids: HashMap<InstanceKey, ContentId, Global> = HashMap::new_in(Global);
    for function in functions.iter().filter(|it| candidate(it)) {
        ids.insert(function.key, code_id(function));
    }
    let candidates = ids.len();
    let span = tracing::info_span!(
        target: "blaze",
        "codegen dedupe",
        functions = functions.len(),
        candidates,
        folded = tracing::field::Empty,
        rounds = tracing::field::Empty,
        bytes = tracing::field::Empty,
    );
    let _entered = span.enter();
    // Unordered: `merged` is sorted as it is made.
    let mut kept: HashMap<InstanceKey, InstanceKey, Global> = HashMap::new_in(Global);
    let (mut rounds, mut bytes) = (0u32, 0u64);
    let mut last = None;
    loop {
        rounds += 1;
        if rounds as usize > candidates + 1 {
            let none = InstanceKey(ContentId::default());
            let merging = last.unwrap_or((none, none));
            return Err(DedupeError::NoFixpoint { rounds: rounds - 1, candidates, merging });
        }
        // Unordered: each group is sorted on its own, and only fills `more`, a map by key.
        let mut groups: HashMap<ContentId, Vec<InstanceKey>, Global> = HashMap::new_in(Global);
        for (key, id) in &ids {
            groups.entry(*id).or_default().push(*key);
        }
        let mut more: HashMap<InstanceKey, InstanceKey, Global> = HashMap::new_in(Global);
        for mut keys in groups.into_values().filter(|it| it.len() > 1) {
            keys.sort();
            let first = keys[0];
            more.extend(keys[1..].iter().map(|key| (*key, first)));
        }
        if more.is_empty() {
            break;
        }
        last = more.iter().min().map(|(from, to)| (*from, *to));
        match originals.as_deref_mut() {
            None => functions.retain(|it| {
                let gone = more.contains_key(&it.key);
                bytes += if gone { it.code.len() as u64 } else { 0 };
                !gone
            }),
            Some(originals) => {
                // alloc: per round that merges, the functions it keeps
                let mut stay = Vec::with_capacity(functions.len());
                for it in std::mem::take(functions) {
                    if !more.contains_key(&it.key) {
                        stay.push(it);
                        continue;
                    }
                    bytes += it.code.len() as u64;
                    // Renamed in a round before: its original is kept already.
                    if copied.insert(it.key) {
                        originals.functions.push(it);
                    }
                }
                *functions = stay;
            }
        }
        for key in more.keys() {
            ids.remove(key);
        }
        for to in kept.values_mut() {
            *to = renamed(to, &more);
        }
        kept.extend(more.iter().map(|(from, to)| (*from, *to)));
        for function in functions.iter_mut() {
            let before = match originals.is_some() && !copied.contains(&function.key) {
                // clone: a function as it was given, when this round may rename it and its
                // original is kept; dropped when the round leaves it as it is
                true if names_any(function, &more) => Some(function.clone()),
                _ => None,
            };
            let changed = rename(function, &more)?;
            if let (true, Some(before), Some(originals)) = (changed, before, originals.as_deref_mut())
            {
                copied.insert(before.key);
                originals.functions.push(before);
            }
            if changed && ids.contains_key(&function.key) {
                ids.insert(function.key, code_id(function));
            }
        }
    }
    let mut renamed_statics = Vec::new(); // alloc: per build, the few statics that name a merged function
    for (at, value) in statics.iter_mut().enumerate() {
        // clone: a static as it was given, when its original is kept and it may be renamed
        let before = (originals.is_some() && static_names_any(value, &kept)).then(|| value.clone());
        if rename_static(value, &kept)? {
            renamed_statics.push(at);
            if let (Some(before), Some(originals)) = (before, originals.as_deref_mut()) {
                originals.statics.push(before);
            }
        }
    }
    if let Some(originals) = originals {
        originals.functions.sort_by_key(|it| it.key);
        originals.statics.sort_by_key(|it| (it.item, it.instance));
    }
    functions.sort_by_key(|it| it.key);
    span.record("folded", kept.len()).record("rounds", rounds).record("bytes", bytes);
    // alloc: the build's answer, sorted once as it is made
    Ok((Deduped { merged: kept.into_iter().collect(), rounds, bytes }, renamed_statics))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Arch, RelocKind, Relocation};

    fn run(
        functions: &mut Vec<CompiledFunction>,
        mergeable: impl Fn(&InstanceKey) -> bool,
    ) -> Deduped {
        dedupe(functions, &mut [], mergeable).unwrap().0
    }

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
            let relocs = vec![Relocation {
                at: 0,
                kind: RelocKind::Abs64,
                target: Target::Direct(key(to)),
                addend: 0,
            }];
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
        run(&mut functions, |it| *it != key(9));
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
        let deduped = run(&mut functions, |_| true);
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
        assert!(run(&mut functions, |_| true).merged.is_empty());
        assert_eq!(functions.len(), 2);
    }

    #[test]
    fn counting_functions_count_apart() {
        let counting = |n| {
            let mut it = function(n, 0x90, &[]);
            it.relocs.push(Relocation {
                at: 0,
                kind: RelocKind::PcRel32,
                target: Target::Counter(key(n)),
                addend: 4,
            });
            it
        };
        let mut functions = vec![counting(2), counting(3), function(7, 0xc3, &[3])];
        assert!(run(&mut functions, |_| true).merged.is_empty());
        assert_eq!(functions.len(), 3);
        assert_eq!(functions[0].relocs.targets(), [Target::Counter(key(2))]);
    }

    /// Instrumented functions keep their own records and counters; ones built with a profile
    /// merge as any code, a mismatched one only with another mismatched.
    #[test]
    fn instrumented_functions_count_apart() {
        let record =
            crate::ProfileRecord { name: [1; 16], name_ref: 2, hash: 3, counters: 1, version: 10 };
        let profiled =
            |n, profile| CompiledFunction { profile: Some(profile), ..function(n, 0x90, &[]) };
        let mut functions = vec![
            profiled(2, crate::Profiled::Counted(record)),
            profiled(3, crate::Profiled::Counted(record)),
        ];
        functions[0].relocs.push(Relocation {
            at: 0,
            kind: RelocKind::PcRel32,
            target: Target::ProfileCounters,
            addend: 4,
        });
        functions[1].relocs.push(Relocation {
            at: 0,
            kind: RelocKind::PcRel32,
            target: Target::ProfileCounters,
            addend: 4,
        });
        assert!(run(&mut functions, |_| true).merged.is_empty());
        assert_eq!(functions.len(), 2);

        let used = |mismatched| crate::Profiled::Used { mismatched, applied: !mismatched };
        let mut functions =
            vec![profiled(2, used(true)), profiled(3, used(true)), profiled(4, used(false))];
        let deduped = run(&mut functions, |_| true);
        assert_eq!(deduped.merged, BTreeMap::from([(key(3), key(2))]));
        assert_eq!(functions.len(), 2);
    }

    #[test]
    fn what_may_not_be_merged_stays_but_its_callers_are_renamed() {
        let mut functions =
            vec![function(2, 0x90, &[]), function(3, 0x90, &[]), function(7, 0xc3, &[3])];
        let deduped = run(&mut functions, |it| *it != key(2));
        assert!(deduped.merged.is_empty());
        assert_eq!(functions.len(), 3);
        let mut functions =
            vec![function(2, 0x90, &[]), function(3, 0x90, &[]), function(7, 0xc3, &[3])];
        run(&mut functions, |it| *it != key(7));
        assert_eq!(
            functions.iter().find(|it| it.key == key(7)).unwrap().relocs.targets(),
            [Target::Direct(key(2))]
        );
    }

    #[test]
    fn data_pointing_at_a_merged_function_gets_its_new_id() {
        let vtable = |to: u8| {
            Data::new(
                8,
                vec![0; 8],
                vec![Relocation {
                    at: 0,
                    kind: RelocKind::Abs64,
                    target: Target::Direct(key(to)),
                    addend: 0,
                }]
                .into(),
            )
        };
        let mut caller = function(9, 0x55, &[]);
        let table = vtable(3);
        caller.relocs.push(Relocation {
            at: 0,
            kind: RelocKind::PcRel32,
            target: Target::Data(table.id),
            addend: -4,
        });
        caller.data.push(table);
        let mut functions = vec![function(2, 0x90, &[]), function(3, 0x90, &[]), caller];
        run(&mut functions, |_| true);
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
        assert_eq!(run(&mut a, |_| true), run(&mut b, |_| true));
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
        let want = run(&mut first, |it| *it != key(7));
        assert!(want.merged.len() > 20, "{}", want.merged.len());
        for step in [7usize, 11, 13, 59] {
            let mut functions = make();
            let len = functions.len();
            let permuted: Vec<CompiledFunction> =
                (0..len).map(|at| functions[at * step % len].clone()).collect();
            functions = permuted;
            assert_eq!(run(&mut functions, |it| *it != key(7)), want);
            assert_eq!(functions, first);
        }
    }

    fn abs64(at: u32, target: Target) -> Relocation {
        Relocation { at, kind: RelocKind::Abs64, target, addend: 0 }
    }

    /// `static S: (fn(), &T)` whose `fn` is 3 and whose `T`, writable, points at 3 too.
    fn holder(item: u8) -> crate::Static {
        let owner = ContentId([item; 32]);
        let inner =
            Data::owned(&owner, 0, 8, vec![0; 8], vec![abs64(0, Target::Function(key(3)))].into());
        crate::Static {
            item: blaze_items::ItemKey(owner),
            instance: ContentId::default(),
            align: 8,
            init: crate::Init::Bytes {
                bytes: vec![0; 16],
                relocs: vec![abs64(0, Target::Function(key(3))), abs64(8, Target::Data(inner.id))]
                    .into(),
            },
            data: vec![inner],
            mutable: false,
            thread_local: false,
            section: None,
            imports: Vec::new(),
        }
    }

    #[test]
    fn a_static_naming_a_folded_function_names_the_kept_one() {
        let mut functions = vec![function(2, 0x90, &[]), function(3, 0x90, &[])];
        let untouched =
            crate::Static { init: crate::Init::Zeroed { size: 8 }, data: Vec::new(), ..holder(30) };
        let mut statics = vec![untouched.clone(), holder(31)];
        let before = holder(31);
        let (deduped, renamed) = dedupe(&mut functions, &mut statics, |_| true).unwrap();
        assert_eq!(deduped.merged, BTreeMap::from([(key(3), key(2))]));
        assert_eq!(renamed, [1], "only the static that named 3 changed");
        assert_eq!(statics[0], untouched);
        let crate::Init::Bytes { relocs, .. } = &statics[1].init else { panic!("bytes") };
        let inner = &statics[1].data[0];
        assert_eq!(relocs.targets(), [Target::Function(key(2)), Target::Data(inner.id)]);
        assert_eq!(inner.relocs.targets(), [Target::Function(key(2))]);
        assert!(inner.writable, "a writable datum stays writable");
        assert_ne!(inner.id, before.data[0].id, "it is another object once it points elsewhere");
        assert_ne!(
            inner.id,
            Data::new(8, vec![0; 8], inner.relocs.clone()).id,
            "and still its static's own"
        );
        let (mut again, mut statics_again) =
            (vec![function(3, 0x90, &[]), function(2, 0x90, &[])], vec![untouched, holder(31)]);
        dedupe(&mut again, &mut statics_again, |_| true).unwrap();
        assert_eq!(statics_again, statics, "renamed the same whatever the order");
    }

    #[test]
    fn a_datum_naming_an_earlier_one_gets_its_new_id_and_a_later_one_is_refused() {
        // 9's inner datum, first, points at 3; its outer one points at the inner one.
        let inner = Data::new(8, vec![1; 8], vec![abs64(0, Target::Direct(key(3)))].into());
        let outer = Data::new(8, vec![2; 8], vec![abs64(0, Target::Data(inner.id))].into());
        let caller = |data: Vec<Data>| {
            let mut it = function(9, 0x55, &[]);
            it.relocs.push(Relocation {
                at: 0,
                kind: RelocKind::PcRel32,
                target: Target::Data(outer.id),
                addend: -4,
            });
            it.data = data;
            it
        };
        let mut functions = vec![
            function(2, 0x90, &[]),
            function(3, 0x90, &[]),
            caller(vec![inner.clone(), outer.clone()]),
        ];
        run(&mut functions, |_| true);
        let inner2 = Data::new(8, vec![1; 8], vec![abs64(0, Target::Direct(key(2)))].into());
        let outer2 = Data::new(8, vec![2; 8], vec![abs64(0, Target::Data(inner2.id))].into());
        let renamed = functions.iter().find(|it| it.key == key(9)).unwrap();
        assert_eq!(renamed.data, [inner2, outer2.clone()]);
        assert_eq!(renamed.relocs.targets(), [Target::Data(outer2.id)]);

        let mut functions = vec![
            function(2, 0x90, &[]),
            function(3, 0x90, &[]),
            caller(vec![outer.clone(), inner.clone()]),
        ];
        let refused = dedupe(&mut functions, &mut [], |_| true).unwrap_err();
        assert_eq!(
            refused,
            DedupeError::DataOrder {
                owner: DataOwner::Function(key(9)),
                datum: outer.id,
                points_at: inner.id
            }
        );
    }

    /// Two chains of 31 that differ in no byte fold one link per round, transitively: the
    /// fixpoint takes one round per link and one more, within its bound.
    #[test]
    fn a_chain_folds_to_its_fixpoint_one_link_a_round() {
        let chain = |from: u8| {
            (0..31u8).map(move |n| {
                if n == 0 {
                    function(from, 0x90, &[])
                } else {
                    function(from + n, 0xcc, &[from + n - 1])
                }
            })
        };
        let mut functions: Vec<CompiledFunction> = chain(1).chain(chain(101)).collect();
        let deduped = run(&mut functions, |_| true);
        assert_eq!(deduped.merged.len(), 31);
        assert!(deduped.merged.iter().all(|(from, to)| from.0.0[0] == to.0.0[0] + 100));
        assert_eq!(deduped.rounds, 32);
        assert_eq!(deduped.bytes, 1 + 30 * 6);
        assert_eq!(functions.len(), 31);
    }
}
