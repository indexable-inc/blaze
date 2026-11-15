//! The model a simulation runs on: a program of generic functions and statics, its front end
//! (body keys, statics' values, spans, checks), and the maker of plans and code heads.
//!
//! The maker makes a code record's head only (what the code refers to after inlining): the
//! code itself is codegen's, and the walk and the row rule read nothing else of it. An inlined
//! callee's calls and statics stand in place of its call, as its code would.

use std::collections::{BTreeMap, BTreeSet};

use blaze_sim::Tape;

use crate::codec::{Digest, Hasher};
use crate::inline::{InlineAttr, InlineEdge, OwnFacts, SizeClass};
use crate::key::{ArgsKey, BodyKey, ItemId, Settings, Symbol, Tier, TypeKey};
use crate::record::{CodeHead, Plan, Reloc, RelocKind, StaticValue, Target};
use crate::walk::{BodyError, Front, Span};

pub(super) const TYPES: u8 = 3;

/// True `num` times in `den`, drawn from the tape; calm (a draw of 0) is false.
pub(super) fn chance(tape: &Tape, num: u64, den: u64, label: &'static str) -> bool {
    tape.below(den, label) >= den.saturating_sub(num)
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

fn attr_of(tape: &Tape) -> InlineAttr {
    match tape.below(10, "attr") {
        0..=4 => InlineAttr::None,
        5 | 6 => InlineAttr::Hint,
        7 | 8 => InlineAttr::Always,
        _ => InlineAttr::Never,
    }
}

impl Model {
    pub(super) fn generate(tape: &Tape) -> Model {
        let count = 6 + tape.below(8, "functions") as usize;
        let tests = vec![1, 2];
        let mut fns = Vec::new();
        for index in 0..count {
            let generics = if index == 0 || tests.contains(&index) { 0 } else { tape.below(3, "generics") as u8 };
            fns.push(ModelFn {
                generics,
                text: 0,
                sig: 0,
                attr: attr_of(tape),
                cost: COSTS[tape.below(4, "cost") as usize],
                cold: chance(tape, 1, 8, "cold"),
                calls: Vec::new(),
                statics: Vec::new(),
                panics: tape.below(3, "panics") as u8,
                alive: true,
                file: tape.below(2, "file") as u8,
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
            let calls = if caller == 0 { 2 + tape.below(2, "calls") } else { tape.below(4, "calls") };
            for _ in 0..calls {
                let call = model.random_call(tape, caller);
                model.fns[caller].calls.push(call);
            }
            if chance(tape, 1, 3, "reads a static") {
                let target = tape.below(3, "static read") as usize;
                model.fns[caller].statics.push(target);
            }
        }
        for _ in 0..3 {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&tape.next_u64("static bytes").to_le_bytes());
            let fn_ref = match chance(tape, 1, 2, "static points at a function") {
                true => model.random_plain(tape),
                false => None,
            };
            model.statics.push(ModelStatic { bytes, fn_ref });
        }
        model
    }

    /// A call from `caller` to a live function, its type arguments drawn.
    pub(super) fn random_call(&self, tape: &Tape, caller: usize) -> Call {
        let live: Vec<usize> = (0..self.fns.len()).filter(|index| self.fns[*index].alive).collect();
        let callee = live[tape.index(live.len(), "callee")];
        let mut args = Vec::new();
        for _ in 0..self.fns[callee].generics {
            let generics = self.fns[caller].generics;
            if generics > 0 && chance(tape, 1, 2, "passes a parameter") {
                args.push(Arg::Param(tape.below(u64::from(generics), "parameter") as u8));
            } else {
                args.push(Arg::Concrete(tape.below(u64::from(TYPES), "type argument") as u8));
            }
        }
        Call { callee, args }
    }

    /// A live non-generic function, not `main`.
    pub(super) fn random_plain(&self, tape: &Tape) -> Option<usize> {
        let plain: Vec<usize> = (1..self.fns.len())
            .filter(|index| self.fns[*index].alive && self.fns[*index].generics == 0)
            .collect();
        if plain.is_empty() {
            return None;
        }
        Some(plain[tape.index(plain.len(), "plain function")])
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
    by_item: BTreeMap<ItemId, usize>,
    instances: BTreeMap<Symbol, (usize, Vec<u8>)>,
    statics: BTreeMap<Symbol, usize>,
}

impl ModelFront {
    pub(super) fn new(model: &Model, settings: Settings) -> ModelFront {
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
        ModelFront { model: model.clone(), settings, by_item, instances, statics }
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

    /// The head of the code of `symbol` at `tier`, taking in the callees of `inline`: its calls
    /// not taken in, and its statics, each once in the order its code would refer to them.
    pub(super) fn make_head(&self, symbol: &Symbol, tier: Tier, inline: &[InlineEdge]) -> CodeHead {
        let _ = tier;
        let mut edges = BTreeSet::new();
        for edge in inline {
            edges.insert((edge.caller, edge.callee));
        }
        let mut head = CodeHead { calls: Vec::new(), statics: Vec::new() };
        self.emit(symbol, &edges, &mut head, 0);
        head
    }

    fn emit(&self, symbol: &Symbol, inline: &BTreeSet<(Symbol, Symbol)>, head: &mut CodeHead, depth: u32) {
        assert!(depth < 64, "an inline tree deeper than 64: the component rule is broken");
        let (index, _) = self.instance(symbol);
        let function = &self.model.fns[index];
        if let Err(error) = self.body_key(&item(index)) {
            panic!("the maker was asked for {symbol}, which has no body: {error:?}");
        }
        for callee in self.plan(symbol).calls {
            if inline.contains(&(*symbol, callee)) {
                self.emit(&callee, inline, head, depth + 1);
            } else if !head.calls.contains(&callee) {
                head.calls.push(callee);
            }
        }
        for target in &function.statics {
            let target = static_symbol(*target);
            if !head.statics.contains(&target) {
                head.statics.push(target);
            }
        }
    }
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
