//! The keys: what an instance is called across edits (its [`Symbol`]), and the two keys that
//! find its records, both computed before any lowering.
//!
//! **Symbol identity.** A function instance is its item (a function, or the subject of a shim:
//! drop glue, a vtable shim, a closure, by the front end's stable id of it, a hash of its path
//! and kind with no position) and the content keys of its type arguments ([`ArgsKey`]). A
//! symbol names the same instance in every revision: a running program's slot, a manifest's
//! row and a relocation all name symbols, never an instance key, so a callee's body edit
//! changes no caller's record.
//!
//! **The plan key** ([`plan_key`]): the item, the body's key (an opaque digest the front end
//! supplies: the item's text with positions item-relative, plus what the names it mentions
//! resolve to, decision 2a), the type arguments' content keys and the settings that change
//! code. It finds the instance's [`Plan`](crate::record::Plan): its calls in MIR order and its
//! own facts. Nothing in it needs any other instance's work.
//!
//! **The instance key** ([`instance_key`]): the plan key and the instance keys of the bodies
//! whose code it takes in (its inlinees, at every tier, each by its own instance key, so the
//! rule is Merkle and transitive: an edit of a small callee moves exactly the keys of the
//! callers that inlined it, at any depth). Which callees are inlined is decided by
//! [`crate::inline`] from records alone (the caller's plan, each callee's own facts), so the
//! key is known before the instance is lowered: the two-step rule.
//!
//! What a type argument's key must cover (the front end's obligation, not checked here): the
//! type's definition and every fact trait selection reads of it for an instance (its impls,
//! by the digest of each), so two instances of one body whose arguments select different
//! impls have two keys (rule keys-cover-every-input). What a body key must cover: everything
//! the instance's calls and code depend on except the type arguments and the inlinees.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::{alloc::Global, fmt, marker::PhantomData};

use blaze_keyed::kind::{Inlinee, Instance, InstanceInputs, InstancePlan, Key, Kind, PlanInputs, Tier as CoreTier};

use crate::codec::{CodecError, Digest, Hasher, In, Out};

/// The domain of a list of type arguments' key.
pub const ARGS_DOMAIN: &str = "blaze program args 1";
/// The domain of the settings' key.
pub const SETTINGS_DOMAIN: &str = "blaze program settings 1";

/// An item (or a shim's subject) by the front end's stable identity: a hash of its path and
/// kind, with no position in it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct ItemId(pub Digest);

/// A body's key, as the front end makes it from text before the body is lowered.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct BodyKey(pub Digest);

/// A type argument's content key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct TypeKey(pub Digest);

/// The key of an instance's list of type arguments, in order.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct ArgsKey(pub Digest);

impl ArgsKey {
    /// No type arguments: a non-generic function, or a static.
    pub const NONE: ArgsKey = ArgsKey(Digest([0; 32]));
    /// The least and greatest keys: the bounds of every instance of one item in a map ordered
    /// by symbol.
    pub const LEAST: ArgsKey = ArgsKey(Digest([0; 32]));
    pub const GREATEST: ArgsKey = ArgsKey(Digest([0xff; 32]));

    /// The key of `args` in order; [`ArgsKey::NONE`] for none.
    pub fn of(args: &[TypeKey]) -> ArgsKey {
        if args.is_empty() {
            return ArgsKey::NONE;
        }
        let mut hasher = Hasher::new(ARGS_DOMAIN);
        hasher.update(&(args.len() as u64).to_le_bytes());
        for arg in args {
            hasher.id(&arg.0);
        }
        ArgsKey(hasher.finish())
    }
}

/// What an instance is called in every revision: its item and its type arguments' key.
/// Ordered by item first, so a map ordered by symbol holds an item's instances together.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct Symbol {
    pub item: ItemId,
    pub args: ArgsKey,
}

impl Symbol {
    pub fn new(item: ItemId, args: ArgsKey) -> Symbol {
        Symbol { item, args }
    }

    /// The least and greatest symbols of `item`: a range over every instance of it.
    pub fn first_of(item: ItemId) -> Symbol {
        Symbol { item, args: ArgsKey::LEAST }
    }

    pub fn last_of(item: ItemId) -> Symbol {
        Symbol { item, args: ArgsKey::GREATEST }
    }

    pub fn encode(&self, out: &mut Out) {
        out.digest(&self.item.0);
        out.digest(&self.args.0);
    }

    pub fn decode(input: &mut In<'_>) -> Result<Symbol, CodecError> {
        Ok(Symbol { item: ItemId(input.digest()?), args: ArgsKey(input.digest()?) })
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.item.0)?;
        if self.args != ArgsKey::NONE {
            write!(f, "<{}>", self.args.0)?;
        }
        Ok(())
    }
}

/// What a program places: a function instance or a static, each by its symbol.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Node {
    Function(Symbol),
    Static(Symbol),
}

impl Node {
    pub fn symbol(&self) -> Symbol {
        match self {
            Node::Function(symbol) | Node::Static(symbol) => *symbol,
        }
    }

    pub fn encode(&self, out: &mut Out) {
        match self {
            Node::Function(symbol) => {
                out.u8(0);
                symbol.encode(out);
            }
            Node::Static(symbol) => {
                out.u8(1);
                symbol.encode(out);
            }
        }
    }

    pub fn decode(input: &mut In<'_>) -> Result<Node, CodecError> {
        match input.u8()? {
            0 => Ok(Node::Function(Symbol::decode(input)?)),
            1 => Ok(Node::Static(Symbol::decode(input)?)),
            _ => Err(input.error("a node of a tag no node has")),
        }
    }
}

impl fmt::Display for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Node::Function(symbol) => write!(f, "function {symbol}"),
            Node::Static(symbol) => write!(f, "static {symbol}"),
        }
    }
}

/// The code generator's tier.
///
/// - `Edit`: the fast backend's (copy-and-patch stencils), the first form of workspace code.
///   Inlining costs more remakes than it saves run time here, so only `#[inline(always)]` is
///   inlined ([`crate::inline::inlines`]).
/// - `Release`: maximally optimized, with cross-function inlining by size class (link-time
///   optimization at function grain). Every dependency and everything from the shared bucket is
///   at this tier always; a workspace instance reaches it by a tier-up
///   ([`crate::program::Program::tier_ups`]).
///
/// A tier never changes behaviour: both tiers compile the same body (one body key, one plan);
/// the tier is in the instance key only.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Tier {
    Edit,
    Release,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Panic {
    Unwind,
    Abort,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum DebugInfo {
    None,
    Lines,
    Full,
}

/// The settings that change an instance's code. `target` is the target's identity (triple and
/// machine descriptor), `features` the digest of the target features the code generator reads,
/// `toolchain` the code generator's own identity (its version and the stencils' or LLVM's).
/// Nothing else: no crate, no requester, no path.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Settings {
    pub target: Digest,
    pub features: Digest,
    pub toolchain: Digest,
    pub tier: Tier,
    pub panic: Panic,
    pub debuginfo: DebugInfo,
    pub overflow_checks: bool,
}

/// The digest of [`Settings`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct SettingsKey(pub Digest);

impl Settings {
    /// These settings at `tier`.
    pub fn at(&self, tier: Tier) -> Settings {
        Settings { tier, ..*self }
    }

    /// The key of the settings a plan depends on: all but the tier. A plan (the calls in MIR
    /// order, the body's own facts) is made before any backend runs, so one plan serves both
    /// tiers; the tier enters the instance key ([`instance_key`]).
    pub fn plan_key(&self) -> SettingsKey {
        self.at(Tier::Edit).key()
    }

    pub fn key(&self) -> SettingsKey {
        let mut hasher = Hasher::new(SETTINGS_DOMAIN);
        hasher.id(&self.target).id(&self.features).id(&self.toolchain);
        hasher.update(&[
            self.tier as u8,
            self.panic as u8,
            self.debuginfo as u8,
            u8::from(self.overflow_checks),
        ]);
        SettingsKey(hasher.finish())
    }

    pub fn encode(&self, out: &mut Out) {
        out.digest(&self.target);
        out.digest(&self.features);
        out.digest(&self.toolchain);
        out.u8(self.tier as u8);
        out.u8(self.panic as u8);
        out.u8(self.debuginfo as u8);
        out.bool(self.overflow_checks);
    }

    pub fn decode(input: &mut In<'_>) -> Result<Settings, CodecError> {
        let target = input.digest()?;
        let features = input.digest()?;
        let toolchain = input.digest()?;
        let tier = match input.u8()? {
            0 => Tier::Edit,
            1 => Tier::Release,
            _ => return Err(input.error("a tier no tier has")),
        };
        let panic = match input.u8()? {
            0 => Panic::Unwind,
            1 => Panic::Abort,
            _ => return Err(input.error("a panic strategy no strategy has")),
        };
        let debuginfo = match input.u8()? {
            0 => DebugInfo::None,
            1 => DebugInfo::Lines,
            2 => DebugInfo::Full,
            _ => return Err(input.error("a debug-info level no level has")),
        };
        let overflow_checks = input.bool()?;
        Ok(Settings { target, features, toolchain, tier, panic, debuginfo, overflow_checks })
    }
}

/// The key of an instance's plan record.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct PlanKey(pub Digest);

/// The key of an instance's code record.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct InstanceKey(pub Digest);

impl fmt::Display for InstanceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The plan key of `symbol` while its body's key is `body`, under `settings`: the core's
/// [`InstancePlan`] key, its one rule.
pub fn plan_key(symbol: &Symbol, body: &BodyKey, settings: &SettingsKey) -> PlanKey {
    let inputs = PlanInputs { item: symbol.item.0, args: symbol.args.0, body: body.0, settings: settings.0, of: PhantomData };
    PlanKey(InstancePlan::key(&inputs).id())
}

impl Tier {
    /// The core's tier: [`Tier::Edit`] is the fast backend's, [`Tier::Release`] the top one.
    pub fn core(self) -> CoreTier {
        match self {
            Tier::Edit => CoreTier::Fast,
            Tier::Release => CoreTier::Top,
        }
    }
}

/// The instance key of the instance planned under `plan`, made at `tier`, that takes in
/// `took_in` (its direct inlinees with their instance keys at the same tier, sorted by symbol,
/// each once). The caller keeps the order ([`crate::inline::Keys::resolve`] does): two lists
/// of one set give one key. The core's [`Instance`] key, its one rule.
pub fn instance_key(plan: &PlanKey, tier: Tier, took_in: &[(Symbol, InstanceKey)]) -> InstanceKey {
    debug_assert!(
        took_in.windows(2).all(|pair| pair[0].0 < pair[1].0),
        "an instance's inlinees are sorted by symbol, each once"
    );
    // The core's inputs name each inlinee in its own row: one list per key, freed on return.
    let mut inlinees = Vec::with_capacity_in(took_in.len(), Global);
    inlinees.extend(took_in.iter().map(|(symbol, key)| Inlinee {
        item: symbol.item.0,
        args: symbol.args.0,
        code: Key::from_id(key.0),
    }));
    let inputs = InstanceInputs { plan: Key::from_id(plan.0), tier: tier.core(), inlinees: &inlinees };
    InstanceKey(Instance::key(&inputs).id())
}
