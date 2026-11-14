//! The model a simulation runs on: a program of generic functions and statics, its front end
//! (body keys, statics' values, spans, checks), and the maker of plans and code records.
//!
//! The code a maker makes is in a small instruction set of 4-byte op words, so the image can be
//! run ([`super::world::trace`]): `HEAD` and the first 8 bytes of the plan key (the same at both
//! tiers), `CALL` and a branch field, `STATIC` and an 8-byte address, `PANIC` and an 8-byte
//! location address and a branch field to the extern `blaze_panic`, `RET`. An inlined callee's
//! ops are emitted in place of its `CALL`, without its `RET`, so a run of an image prints the
//! same events whatever was inlined.

use std::collections::{BTreeMap, BTreeSet};

use crate::inline::{InlineAttr, InlineEdge, OwnFacts, SizeClass};
use crate::key::{ArgsKey, BodyKey, ItemId, Settings, Symbol, Tier, TypeKey, plan_key};
use crate::program::Arch;
use crate::record::{CallerFacts, CodeParts, InstanceCode, LineRow, LocRow, Plan, Reloc, RelocKind, StaticValue, Target};
use crate::codec::{Digest, Hasher};
use crate::walk::{BodyError, Front, Span};

pub(super) const OP_HEAD: u8 = 1;
pub(super) const OP_CALL: u8 = 2;
pub(super) const OP_STATIC: u8 = 3;
pub(super) const OP_PANIC: u8 = 4;
pub(super) const OP_RET: u8 = 0xc3;
/// An arm64 `bl` with a zero offset: the template of every arm64 branch field.
pub(super) const ARM64_BL: u32 = 0x9400_0000;
pub(super) const PANIC_EXTERN: &[u8] = b"blaze_panic";
pub(super) const TYPES: u8 = 3;

/// SplitMix64: a seeded generator, so every run replays from its seed.
pub(super) struct Rng(u64);

impl Rng {
    pub(super) fn new(seed: u64) -> Rng {
        Rng(seed ^ 0x9e37_79b9_7f4a_7c15)
    }

    pub(super) fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    pub(super) fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    pub(super) fn chance(&mut self, num: u64, den: u64) -> bool {
        self.below(den) < num
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Arg {
    Concrete(u8),
    Param(u8),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Call {
    pub(super) callee: usize,
    pub(super) args: Vec<Arg>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct ModelFn {
    pub(super) generics: u8,
    pub(super) text: u64,
    pub(super) sig: u64,
    pub(super) attr: InlineAttr,
    pub(super) cost: u32,
    pub(super) cold: bool,
    pub(super) calls: Vec<Call>,
    pub(super) statics: Vec<usize>,
    pub(super) panics: u8,
    pub(super) alive: bool,
    pub(super) file: u8,
    pub(super) line: u32,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct ModelStatic {
    pub(super) bytes: [u8; 8],
    /// A non-generic function its value points at.
    pub(super) fn_ref: Option<usize>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Model {
    pub(super) fns: Vec<ModelFn>,
    pub(super) statics: Vec<ModelStatic>,
    pub(super) types: [u32; TYPES as usize],
    /// Function 0 is `main`; these are the tests. All are roots, none generic.
    pub(super) tests: Vec<usize>,
    /// A function whose body fails its check.
    pub(super) refused: Option<usize>,
}

const COSTS: [u32; 4] = [10, 40, 80, 200];

fn attr_of(rng: &mut Rng) -> InlineAttr {
    match rng.below(10) {
        0..=4 => InlineAttr::None,
        5 | 6 => InlineAttr::Hint,
        7 | 8 => InlineAttr::Always,
        _ => InlineAttr::Never,
    }
}

impl Model {
    pub(super) fn generate(rng: &mut Rng) -> Model {
        let count = 6 + rng.below(8) as usize;
        let tests = vec![1, 2];
        let mut fns = Vec::new();
        for index in 0..count {
            let generics = if index == 0 || tests.contains(&index) { 0 } else { rng.below(3) as u8 };
            fns.push(ModelFn {
                generics,
                text: 0,
                sig: 0,
                attr: attr_of(rng),
                cost: COSTS[rng.below(4) as usize],
                cold: rng.chance(1, 8),
                calls: Vec::new(),
                statics: Vec::new(),
                panics: rng.below(3) as u8,
                alive: true,
                file: rng.below(2) as u8,
                line: 10 * index as u32 + 1,
            });
        }
        let mut model = Model {
            fns,
            statics: Vec::new(),
            types: [0; TYPES as usize],
            tests,
            refused: None,
        };
        for caller in 0..count {
            let calls = if caller == 0 { 2 + rng.below(2) } else { rng.below(4) };
            for _ in 0..calls {
                let call = model.random_call(rng, caller);
                model.fns[caller].calls.push(call);
            }
            if rng.chance(1, 3) {
                let target = rng.below(3) as usize;
                model.fns[caller].statics.push(target);
            }
        }
        for _ in 0..3 {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&rng.next().to_le_bytes());
            let fn_ref = match rng.chance(1, 2) {
                true => model.random_plain(rng),
                false => None,
            };
            model.statics.push(ModelStatic { bytes, fn_ref });
        }
        model
    }

    /// A call from `caller` to a live function, its type arguments drawn.
    pub(super) fn random_call(&self, rng: &mut Rng, caller: usize) -> Call {
        let live: Vec<usize> = (0..self.fns.len()).filter(|index| self.fns[*index].alive).collect();
        let callee = live[rng.below(live.len() as u64) as usize];
        let mut args = Vec::new();
        for _ in 0..self.fns[callee].generics {
            let generics = self.fns[caller].generics;
            if generics > 0 && rng.chance(1, 2) {
                args.push(Arg::Param(rng.below(u64::from(generics)) as u8));
            } else {
                args.push(Arg::Concrete(rng.below(u64::from(TYPES)) as u8));
            }
        }
        Call { callee, args }
    }

    /// A live non-generic function, not `main`.
    pub(super) fn random_plain(&self, rng: &mut Rng) -> Option<usize> {
        let plain: Vec<usize> = (1..self.fns.len())
            .filter(|index| self.fns[*index].alive && self.fns[*index].generics == 0)
            .collect();
        if plain.is_empty() {
            return None;
        }
        Some(plain[rng.below(plain.len() as u64) as usize])
    }

    pub(super) fn roots(&self) -> Vec<usize> {
        let mut roots = vec![0];
        roots.extend(self.tests.iter().copied());
        roots
    }
}

pub(super) fn item(index: usize) -> ItemId {
    ItemId(Digest::of("blaze program test item 1", &(index as u64).to_le_bytes()))
}

pub(super) fn static_symbol(index: usize) -> Symbol {
    Symbol::new(ItemId(Digest::of("blaze program test static 1", &(index as u64).to_le_bytes())), ArgsKey::NONE)
}

pub(super) fn type_key(model: &Model, ty: u8) -> TypeKey {
    let mut bytes = [0u8; 5];
    bytes[0] = ty;
    bytes[1..].copy_from_slice(&model.types[ty as usize].to_le_bytes());
    TypeKey(Digest::of("blaze program test type 1", &bytes))
}

pub(super) fn fn_symbol(model: &Model, index: usize, args: &[u8]) -> Symbol {
    let mut keys = Vec::new();
    for arg in args {
        keys.push(type_key(model, *arg));
    }
    Symbol::new(item(index), ArgsKey::of(&keys))
}

/// The front end of one revision of a model.
pub(super) struct ModelFront {
    pub(super) model: Model,
    pub(super) settings: Settings,
    pub(super) arch: Arch,
    by_item: BTreeMap<ItemId, usize>,
    instances: BTreeMap<Symbol, (usize, Vec<u8>)>,
    statics: BTreeMap<Symbol, usize>,
}

impl ModelFront {
    pub(super) fn new(model: &Model, settings: Settings, arch: Arch) -> ModelFront {
        let mut by_item = BTreeMap::new();
        let mut instances = BTreeMap::new();
        for (index, function) in model.fns.iter().enumerate() {
            by_item.insert(item(index), index);
            let combos = u32::from(TYPES).pow(u32::from(function.generics));
            for combo in 0..combos {
                let mut args = Vec::new();
                let mut rest = combo;
                for _ in 0..function.generics {
                    args.push((rest % u32::from(TYPES)) as u8);
                    rest /= u32::from(TYPES);
                }
                instances.insert(fn_symbol(model, index, &args), (index, args));
            }
        }
        let mut statics = BTreeMap::new();
        for index in 0..model.statics.len() {
            statics.insert(static_symbol(index), index);
        }
        ModelFront { model: model.clone(), settings, arch, by_item, instances, statics }
    }

    pub(super) fn instance(&self, symbol: &Symbol) -> (usize, Vec<u8>) {
        match self.instances.get(symbol) {
            Some(found) => found.clone(),
            None => panic!("the model has no instance {symbol}"),
        }
    }

    /// The function index and type arguments of `symbol`, if this revision has that instance.
    pub(super) fn find(&self, symbol: &Symbol) -> Option<(usize, Vec<u8>)> {
        self.instances.get(symbol).cloned()
    }

    pub(super) fn static_index(&self, symbol: &Symbol) -> Option<usize> {
        self.statics.get(symbol).copied()
    }

    /// The callee symbol of `call` made from an instance with `args`.
    pub(super) fn callee(&self, call: &Call, args: &[u8]) -> Symbol {
        let mut resolved = Vec::new();
        for arg in &call.args {
            resolved.push(match arg {
                Arg::Concrete(ty) => *ty,
                Arg::Param(param) => args[*param as usize],
            });
        }
        fn_symbol(&self.model, call.callee, &resolved)
    }

    pub(super) fn file_name(file: u8) -> Vec<u8> {
        format!("src/m{file}.rs").into_bytes()
    }

    /// The plan of `symbol`, from its body.
    pub(super) fn plan(&self, symbol: &Symbol) -> Plan {
        let (index, args) = self.instance(symbol);
        let function = &self.model.fns[index];
        let mut calls = Vec::new();
        for call in &function.calls {
            let callee = self.callee(call, &args);
            if !calls.contains(&callee) {
                calls.push(callee);
            }
        }
        Plan {
            symbol: *symbol,
            calls,
            facts: OwnFacts { size: SizeClass::of_cost(function.cost), attr: function.attr, cold: function.cold },
        }
    }

    /// The code of `symbol` at `tier`, taking in the callees of `inline`.
    pub(super) fn make_code(&self, symbol: &Symbol, tier: Tier, inline: &[InlineEdge]) -> InstanceCode {
        let _ = tier;
        let mut edges = BTreeSet::new();
        for edge in inline {
            edges.insert((edge.caller, edge.callee));
        }
        let mut out = Emit::default();
        self.emit(symbol, &edges, &mut out, 0);
        out.code.extend_from_slice(&[OP_RET, 0, 0, 0]);
        let externs = match out.panics {
            true => vec![PANIC_EXTERN.to_vec()],
            false => Vec::new(),
        };
        let size = out.code.len() as u32;
        let parts = CodeParts {
            symbol: *symbol,
            facts: CallerFacts { noreturn: false, nounwind: !out.panics, size },
            code: out.code,
            unwind: vec![0xee; 4],
            relocs: out.relocs,
            locations: out.locations,
            lines: out.lines,
            externs,
        };
        match InstanceCode::new(parts) {
            Ok(code) => code,
            Err(error) => panic!("the model made a code record its checks refuse: {error}"),
        }
    }

    fn branch(&self, out: &mut Emit, target: Target) {
        let at = out.code.len() as u32;
        match self.arch {
            Arch::X86_64 => {
                out.code.extend_from_slice(&[0; 4]);
                out.relocs.push(Reloc { at, kind: RelocKind::Rel32, target, addend: -4 });
            }
            Arch::Arm64 => {
                out.code.extend_from_slice(&ARM64_BL.to_le_bytes());
                out.relocs.push(Reloc { at, kind: RelocKind::Arm64Branch26, target, addend: 0 });
            }
        }
    }

    fn emit(&self, symbol: &Symbol, inline: &BTreeSet<(Symbol, Symbol)>, out: &mut Emit, depth: u32) {
        assert!(depth < 64, "an inline tree deeper than 64: the component rule is broken");
        let (index, args) = self.instance(symbol);
        let function = &self.model.fns[index];
        let body = match self.body_key(&item(index)) {
            Ok(body) => body,
            Err(error) => panic!("the maker was asked for {symbol}, which has no body: {error:?}"),
        };
        let plan = plan_key(symbol, &body, &self.settings.plan_key());
        out.lines.push(LineRow { offset: out.code.len() as u32, item: item(index), line: 0 });
        out.code.extend_from_slice(&[OP_HEAD, 0, 0, 0]);
        out.code.extend_from_slice(&plan.0.0[..8]);
        for callee in self.plan(symbol).calls {
            if inline.contains(&(*symbol, callee)) {
                self.emit(&callee, inline, out, depth + 1);
            } else {
                out.code.extend_from_slice(&[OP_CALL, 0, 0, 0]);
                self.branch(out, Target::Function(callee));
            }
        }
        for target in &function.statics {
            out.code.extend_from_slice(&[OP_STATIC, 0, 0, 0]);
            let at = out.code.len() as u32;
            out.code.extend_from_slice(&[0; 8]);
            out.relocs.push(Reloc { at, kind: RelocKind::Abs64, target: Target::Static(static_symbol(*target)), addend: 0 });
        }
        for panic in 0..function.panics {
            out.code.extend_from_slice(&[OP_PANIC, 0, 0, 0]);
            let at = out.code.len() as u32;
            out.code.extend_from_slice(&[0; 8]);
            let location = out.locations.len() as u32;
            out.locations.push(LocRow { item: item(index), line: u32::from(panic) + 1, col: u32::from(panic) + 5 });
            out.relocs.push(Reloc { at, kind: RelocKind::Abs64, target: Target::Location(location), addend: 0 });
            self.branch(out, Target::Extern(0));
            out.panics = true;
        }
    }
}

#[derive(Default)]
struct Emit {
    code: Vec<u8>,
    relocs: Vec<Reloc>,
    locations: Vec<LocRow>,
    lines: Vec<LineRow>,
    panics: bool,
}

impl Front for ModelFront {
    fn body_key(&self, item: &ItemId) -> Result<BodyKey, BodyError> {
        let Some(index) = self.by_item.get(item) else { return Err(BodyError::Missing) };
        let function = &self.model.fns[*index];
        if !function.alive {
            return Err(BodyError::Missing);
        }
        if self.model.refused == Some(*index) {
            return Err(BodyError::Refused { diagnostics: Digest::of("blaze program test diagnostics 1", &item.0.0) });
        }
        let mut hasher = Hasher::new("blaze program test body 1");
        hasher.update(&function.text.to_le_bytes()).update(&function.sig.to_le_bytes());
        hasher.update(&[function.generics, function.attr as u8, u8::from(function.cold), function.panics]);
        hasher.update(&function.cost.to_le_bytes());
        hasher.update(&(function.calls.len() as u64).to_le_bytes());
        for call in &function.calls {
            let callee = &self.model.fns[call.callee];
            hasher.id(&self::item(call.callee).0).update(&callee.sig.to_le_bytes()).update(&[callee.generics]);
            for arg in &call.args {
                match arg {
                    Arg::Concrete(ty) => {
                        hasher.update(&[0]).id(&type_key(&self.model, *ty).0);
                    }
                    Arg::Param(param) => {
                        hasher.update(&[1, *param]);
                    }
                }
            }
        }
        hasher.update(&(function.statics.len() as u64).to_le_bytes());
        for target in &function.statics {
            hasher.id(&static_symbol(*target).item.0);
        }
        Ok(BodyKey(hasher.finish()))
    }

    fn in_workspace(&self, item: &ItemId) -> bool {
        self.by_item.get(item).is_some_and(|index| index % 3 != 2)
    }

    fn static_value(&self, symbol: &Symbol) -> Option<StaticValue> {
        let index = self.static_index(symbol)?;
        let value = &self.model.statics[index];
        let mut bytes = value.bytes.to_vec();
        let mut relocs = Vec::new();
        if let Some(function) = value.fn_ref {
            relocs.push(Reloc { at: 8, kind: RelocKind::Abs64, target: Target::Function(fn_symbol(&self.model, function, &[])), addend: 0 });
            bytes.extend_from_slice(&[0; 8]);
        }
        match StaticValue::new(bytes, relocs) {
            Ok(value) => Some(value),
            Err(index) => panic!("the model made a static value its checks refuse at relocation {index}"),
        }
    }

    fn span(&self, item: &ItemId) -> Option<Span> {
        let index = self.by_item.get(item)?;
        let function = &self.model.fns[*index];
        Some(Span { file: ModelFront::file_name(function.file), line: function.line })
    }
}
