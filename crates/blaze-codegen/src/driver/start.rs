//! A launched run's code with no walk of its program (docs/v2/plan/design/launcher.md, "The
//! lazy path"): [`Build::start`] makes only what runs before `main` and `main` itself, and
//! every other function is made alone on its first call ([`Build::one`]), each static on the
//! first ask for it ([`Build::one_static`]). Nothing is walked, instantiated ahead, deduped or
//! linked: a cold hello-world made the codegen walk of all it reaches (2,938 ms of 9,727 ms)
//! for some 210 functions it calls.
//!
//! What a launched run does not do ahead, exactly (docs/v2/plan/design/end-state.md, the
//! owner's decision 2). `blaze build`, `check`, `test` and release profiles are not launched:
//! they walk whole and refuse ahead. A launched run still checks ahead every body's type and
//! borrow check (the unit's check, outside this crate) and a symbol defined twice among its
//! roots (here, as [`Build::reach`] does over its walk).
//!
//! - **Deferred to the instance's first call**, where the launcher is told the refusal with
//!   its diagnostic and the program stops: (a) an error made by instantiating (a const
//!   evaluated with the instance's arguments, a layout too large, a transmute between sizes
//!   that differ after substitution); (b) a function whose IR or code the backend cannot make.
//!   Each is the `Err` of [`Build::one`] or [`Build::one_static`] for that instance, naming it.
//! - **Not checked**: (c) the recursion limit over instances (`recursion::check` needs the
//!   walk's paths): a program rustc refuses for it compiles one instance a call until its
//!   stack ends.
//! - **Not applied**: the program's closed dyn facts
//!   ([`crate::backend::Frontend::closed`]) and dedupe (merging equal code; [`super::near`]).
//!
//! Nothing else is deferred.
//!
//! The refinement, tested (`tests::start::a_start_and_its_first_calls_make_the_whole_builds_code`):
//! under [`Mode::Launched`], a start and then [`Build::one`] of every callee and
//! [`Build::one_static`] of every static to a fixed point give, for each key, the function the
//! whole build makes, byte for byte (each place a function's by its key, through its slot or
//! not, as [`super::near`] says), and for each static its value, describing nothing the whole
//! walk does not reach.
//!
//! [`Mode::Launched`]: crate::plan::Mode::Launched

use std::{
    alloc::Global,
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

use blaze_cas::Key;
use blaze_ids::ContentId;
use blaze_items::{CodeHash, ItemKey};

use super::{
    Build, FrontInstance, FrontStatic, Lookup, Node, Out, Put, Sorted, StaticId, StaticOf, error,
    kept,
    near::{Callee, Near, Resolved},
    records, runs_at_start, subject, symbols_defined_twice,
};
use crate::{
    CodegenError, CompiledFunction, InstanceKey, Linkage, Static,
    backend::{Frontend, Prepare},
    plan::Origin,
};

/// One function made alone ([`Build::one`]), with what a launcher's slot of it needs.
pub struct One<I, S> {
    pub key: InstanceKey,
    /// What it is across edits of its body ([`subject`] of its identity and origin).
    pub identity: ContentId,
    /// What a caller's code assumes of it beyond its address ([`crate::backend::Described::abi`]).
    pub abi: ContentId,
    /// The code its instance is of.
    pub code: CodeHash,
    pub item: ItemKey,
    /// The items of it and of every body it builds in, at any depth, sorted, each once: whose
    /// span tables its code's positions name.
    pub items: Vec<ItemKey>,
    /// Its path, as errors and debuggers name it.
    pub name: String,
    /// The symbol it defines and how, where it defines one.
    pub symbol: Option<(String, Linkage)>,
    /// The content key of its describe ([`Frontend::record_key`]), where the front end has one.
    pub ask: Option<blaze_cas::Key>,
    pub function: Arc<CompiledFunction>,
    /// Every function its code refers to, each once, in the order it refers to them: what its
    /// first calls ask for next.
    pub callees: Vec<Callee<I>>,
    /// Every static its code refers to, each once, in that order.
    pub statics: Vec<(S, StaticId)>,
    /// What the caller puts in the store: its template where it was compiled, and the records
    /// of the describes this call made.
    pub puts: Vec<Put>,
}

/// One static made alone ([`Build::one_static`]).
pub struct OneStatic<I, S> {
    pub id: StaticId,
    /// The static [`Build::one_static`] was given (a start's: the root or the static asked
    /// for): what a launch keys its bind record by, beside `id`.
    pub asked: S,
    pub value: Static,
    /// The symbol it defines and how, where it defines one.
    pub symbol: Option<(String, Linkage)>,
    /// Every function its value refers to, each once, in that order.
    pub callees: Vec<Callee<I>>,
    /// Every static its value refers to, each once, in that order.
    pub statics: Vec<(S, StaticId)>,
    /// What the caller puts in the store: its template where it was compiled, and the records
    /// of the describes this call made.
    pub puts: Vec<Put>,
}

/// What a launched run makes before `main` ([`Build::start`]).
pub struct Start<I, S> {
    /// The function whose symbol is the entry's.
    pub entry: InstanceKey,
    /// The entry, then by key each function a static in an initializer or finalizer section
    /// names ([`runs_at_start`]), each once. The records of the describes the start made are
    /// among the entry's puts.
    pub functions: Vec<One<I, S>>,
    /// By id: every root static and every static those and `functions` name, to a fixed point.
    pub statics: Vec<OneStatic<I, S>>,
    /// Every root function, made or not, in the roots' order: what the launcher's slots of the
    /// program's own symbols are.
    pub declared: Vec<Callee<I>>,
}

/// A store a build reads, borrowed whatever its size: what the driver's own reads take.
struct Through<'s, L: ?Sized>(&'s L);

impl<L: Lookup + ?Sized> Lookup for Through<'_, L> {
    fn read(&self, key: &Key) -> Result<Option<&[u8]>, String> {
        self.0.read(key)
    }
}

/// The first function of a start: one already described (the entry, a root) or one asked for.
enum Begin<B: Prepare> {
    Node(Node<B>),
    Asked((FrontInstance<B>, Origin)),
}

/// The first static of a start: a root, already described, or one asked for.
enum BeginStatic<B: Prepare> {
    Node(StaticOf<B>),
    Asked(FrontStatic<B>),
}

impl<B: Prepare> Build<'_, B> {
    /// `asked` (a function and whose code asks for it) made alone: [`Build::near`], then
    /// [`Build::compile_near`]. Only for a launched build, as `near` is. An error made by
    /// instantiating it or by its backend is its `Err`, naming it.
    ///
    /// Its puts hold the records of the describes this call made, so a build that makes
    /// functions alone makes one at a time.
    pub fn one<L: Lookup + ?Sized>(
        &self,
        asked: &(FrontInstance<B>, Origin),
        store: &L,
    ) -> Result<One<FrontInstance<B>, FrontStatic<B>>, CodegenError> {
        let made = self.near(asked).and_then(|near| self.made_one(near, store));
        let records = self.records_made();
        let mut one = made?;
        one.puts.extend(records?);
        one.puts.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(one)
    }

    /// The static `value` made alone: described, its references resolved as a launched
    /// program's (each function through its slot), and its definition read
    /// from the store or compiled. Only for a launched build, as [`Build::near`] is.
    pub fn one_static<L: Lookup + ?Sized>(
        &self,
        value: &FrontStatic<B>,
        store: &L,
    ) -> Result<OneStatic<FrontInstance<B>, FrontStatic<B>>, CodegenError> {
        // alloc: an error's name of the static
        self.launched_only(&format!("the static {value:?}"))?;
        let counts = kept::Counts::default();
        let made = self
            .frontend
            .describe_static(value)
            .and_then(|it| self.static_node(value, it))
            .and_then(|node| self.made_static(node, &counts, store));
        let records = self.records_made();
        let mut one = made?;
        one.puts.extend(records?);
        one.puts.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(one)
    }

    /// A launched run's start, with no walk: every root described and declared; a symbol
    /// defined twice among the roots refused; the entry (the root whose symbol is `entry`),
    /// each function a static in an initializer or finalizer section names
    /// ([`runs_at_start`]) and every root static and the statics those name made, to a fixed
    /// point. Every other function is made on its first call ([`Build::one`]). The describes
    /// run on all cores; each function is compiled on this thread, which reads the store (a
    /// [`Lookup`] is not shared across threads), and a start compiles a few.
    pub fn start<L: Lookup + ?Sized>(
        &self,
        roots: &[(FrontInstance<B>, Origin)],
        static_roots: &[FrontStatic<B>],
        entry: &str,
        store: &L,
    ) -> Result<Start<FrontInstance<B>, FrontStatic<B>>, Vec<CodegenError>> {
        // alloc: an error's name of the entry
        let what = blaze_report::say!(
            "the start of the program whose entry is ",
            blaze_report::say::Code(entry)
        );
        self.launched_only(&what.flat()).map_err(|why| vec![why])?;
        let counts = kept::Counts::default();
        // alloc: none unless the start fails
        let mut errors = Vec::new();
        let made = self.started(roots, static_roots, entry, &counts, store, &mut errors);
        match self.records_made() {
            Ok(records) => match made {
                Some(mut start) if errors.is_empty() => {
                    if let Some(first) = start.functions.first_mut() {
                        first.puts.extend(records);
                        first.puts.sort_by(|a, b| a.key.cmp(&b.key));
                    }
                    return Ok(start);
                }
                _ => {}
            },
            Err(why) => errors.push(why),
        }
        errors.sort_by_key(|it| (it.unit, it.item));
        errors.dedup();
        Err(errors)
    }

    /// [`Build::start`] once its build is known to be launched: `None`, with `errors`, when it
    /// is refused.
    fn started<L: Lookup + ?Sized>(
        &self,
        roots: &[(FrontInstance<B>, Origin)],
        static_roots: &[FrontStatic<B>],
        entry: &str,
        counts: &kept::Counts,
        store: &L,
        errors: &mut Vec<CodegenError>,
    ) -> Option<Start<FrontInstance<B>, FrontStatic<B>>> {
        // (1) Every root described, on all cores, and declared in the roots' order.
        // alloc: per start, its roots' handles
        let asks: Vec<(FrontInstance<B>, Origin)> = roots.to_vec();
        let described = self.until_stopped(asks, errors, |asked| {
            let node = self.described(&asked, 0, counts).and_then(|it| self.node(&asked.0, asked.1, it));
            (asked, node)
        });
        // alloc (each): per start, sized by its roots
        let mut declared = Vec::with_capacity(described.len());
        let mut nodes: blaze_hash::HashMap<InstanceKey, Node<B>, Global> =
            blaze_hash::HashMap::with_capacity_in(described.len(), Global);
        for (asked, node) in described {
            let node = match node {
                Ok(node) => node,
                Err(why) => {
                    errors.push(why);
                    continue;
                }
            };
            let it = &node.described;
            declared.push(Callee {
                key: node.unit.instance.key(),
                identity: subject(it.identity, it.origin),
                abi: it.abi,
                // clone: a defined symbol's name, a few a program
                symbol: it.symbol.clone().map(|name| (name, it.linkage)),
                ask: self.frontend.record_key(&asked.0, &asked.1),
                asked,
            });
            nodes.insert(node.unit.instance.key(), node);
        }
        // alloc: per start, its root statics
        let mut root_statics: blaze_hash::HashMap<StaticId, StaticOf<B>, Global> =
            blaze_hash::HashMap::with_capacity_in(static_roots.len(), Global);
        for value in static_roots {
            match self.frontend.describe_static(value).and_then(|it| self.static_node(value, it)) {
                Ok(node) => {
                    root_statics.insert((node.described.item, node.described.instance), node);
                }
                Err(why) => errors.push(why),
            }
        }
        if !errors.is_empty() {
            return None;
        }
        // (2) One definition a symbol: every symbol is a root's, so the roots say it all.
        let (nodes, root_statics) = (Sorted::of(nodes), Sorted::of(root_statics));
        symbols_defined_twice::<B>(&nodes, &root_statics, errors);
        if !errors.is_empty() {
            return None;
        }
        // (3) The entry.
        let entry_node = nodes.values().find(|it| it.described.symbol.as_deref() == Some(entry));
        let Some(entry_node) = entry_node else {
            let none = (InstanceKey(ContentId::default()), ItemKey(ContentId::default()));
            // alloc: an error
            let what = blaze_report::say!("the build defines no ", blaze_report::say::Code(entry), ": it is no program");
            errors.push(error(none.0, none.1, what.flat()));
            return None;
        };
        let entry_key = entry_node.unit.instance.key();
        // (4, 5) The entry and what start sections name, with the statics, to a fixed point.
        // alloc (each): per start, what it makes
        let mut asked_functions: HashSet<(FrontInstance<B>, Origin)> = HashSet::new();
        // clone: the entry's ask, held so no callee asks for it again
        asked_functions.insert((entry_node.instance.clone(), entry_node.requester));
        // clone: the entry's node, which the start's first function is made from
        // alloc: per start, what it makes next
        let mut functions_next = vec![Begin::<B>::Node(entry_node.clone())];
        // alloc: per start, its statics
        let mut seen_statics: HashSet<StaticId> = root_statics.keys().copied().collect();
        // clone: the root statics' nodes, each made once
        // alloc: per start, its root statics
        let mut statics_next: Vec<BeginStatic<B>> =
            root_statics.values().map(|it| BeginStatic::Node(it.clone())).collect();
        let mut functions: BTreeMap<InstanceKey, One<FrontInstance<B>, FrontStatic<B>>> = BTreeMap::new();
        let mut statics: BTreeMap<StaticId, OneStatic<FrontInstance<B>, FrontStatic<B>>> = BTreeMap::new();
        loop {
            if !functions_next.is_empty() {
                let nears = self.until_stopped(std::mem::take(&mut functions_next), errors, |begin| match begin {
                    Begin::Node(node) => self.near_from(node, counts),
                    Begin::Asked(asked) => self
                        .described(&asked, 0, counts)
                        .and_then(|it| self.node(&asked.0, asked.1, it))
                        .and_then(|node| self.near_from(node, counts)),
                });
                for near in nears {
                    let one = match near.and_then(|near| self.made_one(near, store)) {
                        Ok(one) => one,
                        Err(why) => {
                            errors.push(why);
                            continue;
                        }
                    };
                    for (value, id) in &one.statics {
                        if seen_statics.insert(*id) {
                            statics_next.push(BeginStatic::Asked(value.clone())); // clone: the ask's handle
                        }
                    }
                    functions.entry(one.key).or_insert(one);
                }
            }
            while let Some(begin) = statics_next.pop() {
                let made = match begin {
                    BeginStatic::Node(node) => self.made_static(node, counts, store),
                    BeginStatic::Asked(value) => self
                        .frontend
                        .describe_static(&value)
                        .and_then(|it| self.static_node(&value, it))
                        .and_then(|node| self.made_static(node, counts, store)),
                };
                let one = match made {
                    Ok(one) => one,
                    Err(why) => {
                        errors.push(why);
                        continue;
                    }
                };
                for (value, id) in &one.statics {
                    if seen_statics.insert(*id) {
                        statics_next.push(BeginStatic::Asked(value.clone())); // clone: the ask's handle
                    }
                }
                if one.value.section.as_deref().is_some_and(runs_at_start) {
                    for callee in &one.callees {
                        if asked_functions.insert(callee.asked.clone()) {
                            // clone: the ask's handle, held so it is asked once
                            functions_next.push(Begin::Asked(callee.asked.clone()));
                        }
                    }
                }
                statics.insert(one.id, one);
            }
            if functions_next.is_empty() || !errors.is_empty() {
                break;
            }
        }
        if !errors.is_empty() {
            return None;
        }
        let Some(first) = functions.remove(&entry_key) else {
            let what = "the start made no function of its entry, whose describe it has";
            errors.push(error(entry_key, nodes[&entry_key].unit.item, what));
            return None;
        };
        // alloc: per start, what it made
        let mut made = Vec::with_capacity(functions.len() + 1);
        made.push(first);
        made.extend(functions.into_values());
        Some(Start { entry: entry_key, functions: made, statics: statics.into_values().collect(), declared })
    }

    /// The records of the describes made since they were last taken, sorted, each key once
    /// ([`records::sorted`]).
    fn records_made(&self) -> Result<Vec<Put>, CodegenError> {
        // alloc: per call, the records its describes made
        let made: Vec<Put> = std::iter::from_fn(|| self.recorded.pop()).collect();
        if made.is_empty() {
            return Ok(made);
        }
        records::sorted(made)
    }

    /// `near`'s function compiled, with what a launcher's slot of it needs: no records.
    fn made_one<L: Lookup + ?Sized>(
        &self,
        near: Near<FrontInstance<B>, FrontStatic<B>>,
        store: &L,
    ) -> Result<One<FrontInstance<B>, FrontStatic<B>>, CodegenError> {
        let (function, puts) = self.compile_near(&near, store)?;
        let node = near.node();
        let it = &node.described;
        let made = (
            subject(it.identity, it.origin),
            it.abi,
            node.unit.instance.code,
            node.unit.item,
            // clone: its path, once a function made
            it.name.clone(),
            // clone: a defined symbol's name, a few a program
            it.symbol.clone().map(|name| (name, it.linkage)),
            self.frontend.record_key(&node.instance, &node.requester),
        );
        let (identity, abi, code, item, name, symbol, ask) = made;
        let items = near.items();
        Ok(One {
            key: near.key,
            identity,
            abi,
            code,
            item,
            items,
            name,
            symbol,
            ask,
            // alloc: once a function made, shared with whoever holds its code
            function: Arc::new(function),
            callees: near.callees,
            statics: near.statics,
            puts,
        })
    }

    /// The static `node` is of, its places resolved and its definition made: no records.
    fn made_static<L: Lookup + ?Sized>(
        &self,
        mut node: StaticOf<B>,
        counts: &kept::Counts,
        store: &L,
    ) -> Result<OneStatic<FrontInstance<B>, FrontStatic<B>>, CodegenError> {
        let mut resolved = Resolved::<B>::new();
        let (refs, _) = self.resolve(&node.described.refs, node.described.origin, counts, &mut resolved)?;
        node.refs = refs;
        let id = (node.described.item, node.described.instance);
        // clone: a defined symbol's name, a few a program
        let symbol = node.described.symbol.clone().map(|name| (name, node.described.linkage));
        let asked = node.value.clone(); // clone: the static's handle, once a static made
        let (unit, item) = (InstanceKey(node.described.value), node.described.item);
        let one = Sorted::of_sorted(std::iter::once((id, node)));
        let mut out = Out::default();
        self.statics(&one, None, &Through(store), &mut out);
        if let Some(why) = out.errors.into_iter().next() {
            return Err(why);
        }
        let (made, Some(value)) = (out.statics.len(), out.statics.pop()) else {
            return Err(error(unit, item, "made no definition of the static"));
        };
        if made != 1 {
            // alloc: an error
            return Err(error(unit, item, format!("made {made} definitions of the static {id:?}, not one")));
        }
        Ok(OneStatic {
            id,
            asked,
            value,
            symbol,
            callees: resolved.callees,
            statics: resolved.named_statics,
            // alloc: per static made, what it puts
            puts: out.puts.into_iter().map(|(_, put)| put).collect(),
        })
    }
}
