//! Slots made as symbols appear (run.md, "End state", "Slot table": "a run has no list of
//! functions made before it starts"; piece 4 of its "Pieces").
//!
//! A code record names its targets by SYMBOL (blaze-code `record::Target`), never by slot. The
//! parent numbers the slots:
//!
//! - **[`Parent::declare`]** gives an instance its slot: the next one the first time, the same
//!   one every time after, whichever record or thread names it first. Slot ids are dense and
//!   follow the order the machine is fed its events, so one schedule numbers alike on any
//!   machine. A second code identity for a declared instance is refused ([`Error::TwoCodes`]);
//!   past the program's reservation, [`Error::Full`].
//! - **The publish path** ([`Parent::looked_named`], [`Parent::compiled_named`]) resolves each
//!   target of the record about to be sent ([`Resolve`]), declares it, and pushes
//!   [`Effect::Declare`] for each (symbol, slot) pair the program was not told yet, ALL before
//!   the record's [`Effect::Publish`]: placing a record never meets a symbol without a slot. A
//!   target that resolves to nothing fails the function ([`Why::Unresolved`], naming it): the
//!   record is not sent.
//! - **A need by symbol** ([`Parent::need_symbol`]): the program asks for a symbol it has no slot
//!   for; it is declared, told, then needed as by slot. A symbol the manifest does not resolve is
//!   answered [`Effect::Unresolved`], naming it; nothing else changes.
//!
//! A refusal part-way through a record's targets leaves the slots declared before it unasked
//! (and told): a declared slot nobody calls costs a row and is never compiled.

#![blaze::sans_io]

use blaze_hash::{FixedState, Map, MapOpts};
use blaze_own::{AVec, Alloc, Vec};

use super::{Compiled, Config, Effect, Error, FastRecord, Function, Parent, Row, State};
use crate::first_call::Symbol;
use crate::tier::{Inlined, usable_planted};
use crate::{CodeRef, CompileError, Hooks, InstanceKey, Nanos, Plant, RecordKey, SlotId, ThreadId, Tier, Why};

/// The program's manifest as the parent reads it: which function a code record's symbol names.
/// A pure lookup (the manifest is a record of this snapshot); `None` when it names none.
pub trait Resolve {
    fn resolve(&self, symbol: Symbol) -> Option<Function>;
}

/// What a lookup found, with the record's targets by symbol: the fast record, and the optimized
/// one by its address, if either is there.
#[derive(Clone, Copy, Debug)]
pub struct Named<'a> {
    pub fast: CodeRef,
    /// The fast record's function targets (blaze-code `Space::Func`), in the record's order.
    pub targets: &'a [Symbol],
    pub optimized: Option<NamedOptimized<'a>>,
}

/// An optimized record found by its address: what it inlined, and its own targets (inlining
/// changes which functions it still calls).
#[derive(Clone, Copy, Debug)]
pub struct NamedOptimized<'a> {
    pub code: CodeRef,
    pub inlined: &'a [Inlined],
    pub targets: &'a [Symbol],
}

/// How one entry of a compile ended, with the record's targets by symbol.
#[derive(Clone, Copy, Debug)]
pub enum CompiledNamed<'r> {
    Record { code: CodeRef, targets: &'r [Symbol] },
    Need(&'r [RecordKey]),
    Failed(Why),
}

/// The parent's names: each instance's slot, the pairs the program was told, and the most slots
/// the program's reservation holds. Looked up only, never iterated: no order reaches an output.
pub(crate) struct Names<'a> {
    by_instance: Map<InstanceKey, SlotId, FixedState, Alloc<'a>>,
    told: Map<Symbol, SlotId, FixedState, Alloc<'a>>,
    most: u32,
}

impl<'a> Names<'a> {
    /// The names of a parent started with `functions` in slots `0..`: no slot past them unless
    /// it was made by [`Parent::growing`].
    pub(crate) fn new(functions: &[Function], alloc: Alloc<'a>) -> Names<'a> {
        let mut by_instance = Map::new(MapOpts { hasher: FixedState, alloc: alloc, capacity: None });
        for (at, function) in functions.iter().enumerate() {
            by_instance.insert(function.instance, SlotId(at as u32));
        }
        Names { by_instance, told: Map::new(MapOpts { hasher: FixedState, alloc: alloc, capacity: None }), most: functions.len() as u32 }
    }
}

impl<'a> Parent<'a> {
    /// The parent of a program whose slots are made as symbols appear: `functions` (the run
    /// record's, or none) in slots `0..`, then each new instance [`Parent::declare`]d, up to
    /// `most_slots`, the slots the program's reservation holds.
    pub fn growing(functions: &[Function], config: Config, most_slots: u32, alloc: Alloc<'a>) -> Result<Parent<'a>, Error> {
        if (most_slots as usize) < functions.len() {
            return Err(Error::Config {
                what: "a reservation of fewer slots than the functions it starts with",
                value: u64::from(most_slots),
            });
        }
        let mut parent = Parent::new(functions, config, alloc)?;
        parent.names.most = most_slots;
        Ok(parent)
    }

    /// The slots declared so far: they are `0..declared()`.
    pub fn declared(&self) -> u32 {
        self.slots()
    }

    /// `function`'s slot: the next one the first time its instance is declared, the same one
    /// every time after. Idempotent per instance; the new slot is `Unasked` and wanted by no one
    /// until a need, a start or a publish's callees name it.
    pub fn declare(&mut self, function: Function) -> Result<SlotId, Error> {
        let known = match self.plant {
            Plant::DeclareTwice => None,
            _ => self.names.by_instance.get(&function.instance).copied(),
        };
        if let Some(slot) = known {
            if self.table.functions()[slot.index()].code != function.code {
                return Err(Error::TwoCodes { instance: function.instance, slot });
            }
            return Ok(slot);
        }
        if self.slots() >= self.names.most {
            return Err(Error::Full { most: self.names.most });
        }
        let slot = SlotId(self.slots());
        self.table.push(Row { function, state: State::Unasked, hooks: Hooks::NONE });
        self.ids.insert(function.instance, function.code);
        self.names.by_instance.insert(function.instance, slot);
        Ok(slot)
    }

    /// `symbol`'s slot, declared if its instance is new; the pair goes into `told` the first time
    /// the program is to learn it. `Ok(Err(symbol))` when the manifest does not resolve it.
    fn name(
        &mut self,
        symbol: Symbol,
        resolve: &impl Resolve,
        told: &mut AVec<'_, Effect>,
    ) -> Result<Result<SlotId, Symbol>, Error> {
        let Some(function) = resolve.resolve(symbol) else {
            return Ok(Err(symbol));
        };
        let slot = self.declare(function)?;
        if self.names.told.get(&symbol) != Some(&slot) {
            self.names.told.insert(symbol, slot);
            told.push(Effect::Declare { symbol, slot });
        }
        Ok(Ok(slot))
    }

    /// Names each of `targets` into `callees`, the new pairs into `told`; the first symbol the
    /// manifest does not resolve stops it.
    fn name_all(
        &mut self,
        targets: &[Symbol],
        resolve: &impl Resolve,
        callees: &mut AVec<'_, SlotId>,
        told: &mut AVec<'_, Effect>,
    ) -> Result<Result<(), Symbol>, Error> {
        for target in targets {
            match self.name(*target, resolve, told)? {
                Ok(slot) => callees.push(slot),
                Err(symbol) => return Ok(Err(symbol)),
            }
        }
        Ok(Ok(()))
    }

    /// `thread` called a stub of `symbol`, for which the program has no slot yet: declared and
    /// told (one [`Effect::Declare`] before anything else about it), then needed as
    /// [`Parent::need`] needs a slot. A symbol the manifest does not resolve is answered
    /// [`Effect::Unresolved`] and changes nothing.
    pub fn need_symbol(
        &mut self,
        thread: ThreadId,
        symbol: Symbol,
        resolve: &impl Resolve,
        now: Nanos,
        out: &mut AVec<'_, Effect>,
    ) -> Result<(), Error> {
        match self.name(symbol, resolve, out)? {
            Ok(slot) => self.need(thread, slot, now, out),
            Err(symbol) => {
                out.push(Effect::Unresolved { thread, symbol });
                Ok(())
            }
        }
    }

    /// [`Parent::looked`] for a store whose records name their targets by symbol: the record
    /// about to be published has each target declared and each new pair told first.
    pub fn looked_named(
        &mut self,
        slot: SlotId,
        found: Option<Named<'_>>,
        resolve: &impl Resolve,
        out: &mut AVec<'_, Effect>,
    ) -> Result<(), Error> {
        self.check(slot)?;
        let State::Looking { .. } = self.state(slot) else {
            return Err(Error::NotLooking { slot });
        };
        let Some(found) = found else {
            return self.looked(slot, None, out);
        };
        let optimized = found.optimized.filter(|it| usable_planted(it.inlined, &*self, self.plant).is_ok());
        let (tier, code, targets) = match optimized {
            Some(optimized) => (Tier::Optimized, optimized.code, optimized.targets),
            None => (Tier::Fast, found.fast, found.targets),
        };
        let mut callees = Vec::with_capacity_in(targets.len(), self.alloc);
        let mut told = Vec::new_in(self.alloc);
        if let Err(symbol) = self.name_all(targets, resolve, &mut callees, &mut told)? {
            out.extend_from_slice(&told);
            let error = CompileError { function: self.instance(slot), slot, why: Why::Unresolved { symbol } };
            self.fail(slot, error, out);
            self.pump(out);
            return Ok(());
        }
        let late = self.plant == Plant::PublishBeforeTell;
        if !late {
            out.extend_from_slice(&told);
        }
        self.has_code(slot, tier, code, &callees, out);
        if late {
            out.extend_from_slice(&told);
        }
        self.pump(out);
        Ok(())
    }

    /// [`Parent::compiled`] for a compiler whose records name their targets by symbol: a record
    /// has each target declared and each new pair told before it is published.
    pub fn compiled_named(
        &mut self,
        slot: SlotId,
        ended: CompiledNamed<'_>,
        now: Nanos,
        resolve: &impl Resolve,
        out: &mut AVec<'_, Effect>,
    ) -> Result<(), Error> {
        let (code, targets) = match ended {
            CompiledNamed::Need(keys) => return self.compiled(slot, Compiled::Need(keys), now, out),
            CompiledNamed::Failed(why) => return self.compiled(slot, Compiled::Failed(why), now, out),
            CompiledNamed::Record { code, targets } => (code, targets),
        };
        self.check(slot)?;
        match self.state(slot) {
            State::Compiling { .. } | State::Failed { compiling: Some(_), .. } => {}
            _ => return Err(Error::NotCompiling { slot }),
        }
        let mut callees = Vec::with_capacity_in(targets.len(), self.alloc);
        let mut told = Vec::new_in(self.alloc);
        if let Err(symbol) = self.name_all(targets, resolve, &mut callees, &mut told)? {
            out.extend_from_slice(&told);
            return self.compiled(slot, Compiled::Failed(Why::Unresolved { symbol }), now, out);
        }
        let record = FastRecord { code, callees, reads: Vec::new_in(self.alloc) };
        let late = self.plant == Plant::PublishBeforeTell;
        if !late {
            out.extend_from_slice(&told);
        }
        self.compiled(slot, Compiled::Record(&record), now, out)?;
        if late {
            out.extend_from_slice(&told);
        }
        Ok(())
    }
}
