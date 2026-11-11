//! Eval: who decides how a pure function is computed (docs/v2/plan/design/std.md, "Eval").
//!
//! A call of a pure function equals its value, so how the value is got is free to choose: run
//! the body, look it up, record where it came from. [`Eval`] makes that choice a value a
//! program passes down, like [`Cx`].
//!
//! ```ignore
//! #[blaze::memo]
//! fn layout(_: &mut Nested<'_>, (ty, target): (&Type, &Target)) -> Layout { .. }
//!
//! #[blaze::memo]
//! fn size_of(points: &mut Nested<'_>, (s, target): (&Struct, &Target)) -> u64 {
//!     s.fields.iter().map(|f| points.call(layout, (&f.ty, target)).size).sum()
//! }
//!
//! let eval = Eval::memo(&io.need_mem()?, MemoLimit { entries: 1 << 16, bytes: 64 << 20 })?;
//! let size = eval.call(cx, size_of, (&s, &target));
//! ```
//!
//! - **A memo point** is a function marked `#[blaze::memo]`, which the compiler holds to
//!   `#[blaze::pure]` (blaze-lints' `blaze::pure`, naming the function and the rule) and whose
//!   item type alone is [`MemoPoint`], a trait only the trait solver implements: [`Eval::call`]
//!   takes nothing else. Its shape is `fn(&mut Nested<'_>, A) -> R`: [`Nested`] is the
//!   evaluator as a running point sees it, for the points it calls, and `A` its arguments.
//! - **[`Point`]** is the point's code identity: the behaviour identity of the function's
//!   instance (`core::reflect::code_id_of::<F>()`, answered by const eval from
//!   `hir_ty_identity::fn_code_id`), over everything it can run. A changed body, here or three
//!   calls down, is another point (rule keys-cover-every-input). Never its name.
//! - **[`Key`]** is the arguments' content ID ([`EvalKey`]) under [`KEY_DOMAIN`]. It is made
//!   only when a provider needs it: [`Eval::direct`] makes none.
//! - **One type with providers** (std.md, "Capabilities are layered, and each has providers"):
//!   [`Eval::direct`] calls the body, [`Eval::memo`] remembers results in arenas of [`Mem`],
//!   [`Eval::handler`] hands every call to a [`Handler`] of the caller's. Code takes `&Eval` and
//!   cannot tell which.
//! - **[`Provenance`]** is a provided handler: it wraps another evaluator and records, for each
//!   point run, the points it called, into a [`Graph`] its caller owns.
//!
//! # The shapes chosen, and why
//!
//! **The result is `R: Clone + 'static`, returned by value.** [`Eval::direct`] moves the
//! body's value out: no copy, no allocation, so the marks cost a plain call. [`Eval::memo`]
//! clones once per call (a miss keeps a clone, a hit hands one out). The cost is in the
//! signature: a caller whose result is large makes `R` a [`Shared`](crate::sync::Shared) of it,
//! and the clone is a count. Returning a shared handle from every provider would make `direct`
//! allocate one per call. `'static`, because a kept value outlives the call's arguments.
//!
//! **A handler never holds a typed value.** A call's types are erased behind a private trait
//! whose one implementation knows `F`, `A` and `R`: the value stays in that typed slot, and a
//! handler reaches it as `&dyn Any` ([`Next::value`], [`Next::supply`], a checked downcast that
//! fails with [`Mistyped`]). No `unsafe`, no allocation on the global heap, and a handler
//! cannot change an answer by accident: it decides only where the value comes from
//! ([`Next::compute`], [`Next::through`], [`Next::supply`]).
//!
//! **The current parent is carried by [`Nested`], explicitly.** The `&mut Nested` a point's
//! body is handed holds the evaluator the program called, the task's context and the call that
//! is running; a nested [`Nested::call`] passes that call on as its parent ([`At::parent`]).
//! Nothing is read from a thread-local or a global "current point", and not from [`Cx`]
//! either: a context is shared by everything a task does, so a parent kept there would be
//! shared state written on every call, where `Nested` is a value on the caller's stack.
//!
//! **Why `&mut Nested`, and not `&Eval`.** A memo point is checked `#[blaze::pure]`, and that
//! check scans the point's own parameter types (docs/v2/plan/design/pure-fns.md): state with
//! interior mutability behind a shared reference is refused, as another thread could change it
//! during the call. An evaluator that remembers is such state. What the check lets through is
//! the kernel's door: a `&mut dyn` of a trait marked `#[blaze::pure(trusted = "..")]`. `Nested`
//! holds exactly that, and [`Nested::call`] is trusted for the same reason: whichever evaluator
//! answers, the value is the function's of its arguments. The functions it is given are still
//! walked.
//!
//! # Memory
//!
//! [`Eval::memo`] keeps its entries in two generations, each a map ([`Map`]) and its values in
//! an arena of [`Mem`] of half the byte limit. A new entry goes to the young one; when that
//! holds half the entry limit, or its arena is full, the old generation's entries are dropped
//! whole, its arena is reset, and the two swap. So no entry is freed piecemeal (an arena is a
//! bump allocator), an entry read often and one read once age alike (a hit does not move an
//! entry: moving it would need a second copy), and the limits hold at every moment
//! ([`MemoStats`]). The byte limit counts the entries themselves; what a value owns elsewhere
//! (a `Shared`, a collection in another allocator) is its owner's to count, until results are
//! encoded ([`bstd::data`](crate), not built yet). The two global allocations are the owners of
//! the two arenas' frames, once per evaluator.
//!
//! An `Eval` that remembers is not `Send`, as [`Cx`] is not: its arenas are a task's.
//!
//! # Left out, and what each waits for
//!
//! - `Eval::stored(..)`, over the store and the remote cache: a kept entry there is bytes
//!   (blaze-kernel's `Entry`), and a result has no encoding until `bstd::data` exists.
//! - A generic `EvalKey` for structs by reflection: no crate in the tree uses `for const` field
//!   loops outside tests yet, so a struct's key is a hand-written impl over its fields.
//! - `f32` and `f64` keys: equal values must give equal keys, and `0.0 == -0.0` with two bit
//!   patterns; a caller picks the bits (`to_bits`) or a canonical form.
//! - A point that calls itself, directly or through others: its body would hold its own
//!   [`Point`] as a constant, which code identity refuses (`hir_ty_identity::SELF_READ`).

use alloc::{boxed::Box, vec::Vec};
use core::{alloc::AllocError, any::Any, cell::Cell, fmt};

use blaze_mem::Frame;

use crate::{
    Plant, Stopped,
    cx::Cx,
    id::{ContentHasher, ContentId},
    map::Map,
    mem::{Arena, Mem},
    sync::{LocalLock, LocalShared},
};

/// A function marked `#[blaze::memo]`: its item type, and nothing else. The trait solver
/// implements it (`core::reflect::MemoPoint`, a lang item); no impl of it can be written.
pub use core::reflect::MemoPoint;

/// The domain of a [`Key`]: the arguments of one call of a memo point.
pub const KEY_DOMAIN: &str = "bstd eval key 1";

/// The identity of a memo point: the code identity of its function's instance, over
/// everything it can run. Two points are equal when their code is; [`Point::path`] is for
/// messages only.
#[derive(Clone, Copy)]
pub struct Point {
    code: ContentId,
    path: &'static str,
}

impl Point {
    /// The point of the memo point `f`.
    pub fn of<F: MemoPoint>(f: &F) -> Point {
        let _ = f;
        Point::of_type::<F>()
    }

    /// The point of the memo point whose item type is `F`: a constant of each instance.
    pub fn of_type<F: MemoPoint>() -> Point {
        // A constant of each instance of `F`: its value is in the code identity of whatever
        // names it, so a point's own identity covers the points it calls.
        let code = const { core::reflect::code_id_of::<F>() };
        Point { code: ContentId(code), path: core::any::type_name::<F>() }
    }

    /// Its code identity.
    pub fn code(&self) -> ContentId {
        self.code
    }

    /// The function's path with its generic arguments, as an error names it.
    pub fn path(&self) -> &'static str {
        self.path
    }

    /// The point every entry is kept under when [`Plant::MemoSharesPoints`] is planted.
    fn none() -> Point {
        Point { code: ContentId([0; 32]), path: "" }
    }
}

impl PartialEq for Point {
    fn eq(&self, other: &Point) -> bool {
        self.code == other.code
    }
}

impl Eq for Point {}

impl core::hash::Hash for Point {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.code.hash(state);
    }
}

impl fmt::Debug for Point {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({:?})", self.path, self.code)
    }
}

impl fmt::Display for Point {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}`", self.path)
    }
}

/// The key of a call's arguments: their content ID under [`KEY_DOMAIN`]. Deterministic: the
/// same in every process and on every machine, as no address and no map's order reaches it.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key(ContentId);

impl Key {
    /// The key of `args`.
    pub fn of<A: EvalKey + ?Sized>(args: &A) -> Key {
        Key::written(args, false)
    }

    fn written<A: EvalKey + ?Sized>(args: &A, skips_last: bool) -> Key {
        let mut into = KeyWriter { hasher: ContentHasher::new(KEY_DOMAIN), skips_last };
        args.write_key(&mut into);
        Key(into.hasher.finish())
    }

    /// The content ID it is.
    pub fn id(&self) -> ContentId {
        self.0
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Key({:?})", self.0)
    }
}

/// What an [`EvalKey`] writes itself into: the hasher of one [`Key`].
pub struct KeyWriter {
    hasher: ContentHasher,
    /// [`Plant::KeyIgnoresLast`]: a tuple leaves its last element out.
    skips_last: bool,
}

impl KeyWriter {
    /// `bytes` as they are. A value of a fixed width writes its bytes; one of any length
    /// writes [`KeyWriter::count`] first, so two values never run together.
    pub fn bytes(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
    }

    /// How many elements or bytes follow, at one width on every target.
    pub fn count(&mut self, count: usize) {
        self.bytes(&(count as u64).to_le_bytes());
    }

    /// The last element of a tuple.
    fn last<T: EvalKey + ?Sized>(&mut self, value: &T) {
        if !self.skips_last {
            value.write_key(self);
        }
    }
}

/// A value that can be part of a [`Key`]: it writes what it is, and nothing about where it
/// is. Equal values write equal bytes, and two values of one type that differ write bytes that
/// differ and that are no prefix of each other's (a fixed width, or a count first), so the
/// parts of a tuple never run together. There is no impl for a raw pointer, a function pointer
/// or anything else whose value is an address, and none for a map whose order is its hasher's.
/// A struct writes its fields in declaration order by hand.
pub trait EvalKey {
    fn write_key(&self, into: &mut KeyWriter);

    /// `items` one after another, without their count. `u8` writes them as one run of bytes.
    fn write_keys(items: &[Self], into: &mut KeyWriter)
    where
        Self: Sized,
    {
        for item in items {
            item.write_key(into);
        }
    }
}

macro_rules! int_keys {
    ($($int:ty),+) => {$(
        impl EvalKey for $int {
            fn write_key(&self, into: &mut KeyWriter) {
                into.bytes(&self.to_le_bytes());
            }
        }
    )+};
}

int_keys!(u16, u32, u64, u128, i8, i16, i32, i64, i128);

impl EvalKey for u8 {
    fn write_key(&self, into: &mut KeyWriter) {
        into.bytes(&[*self]);
    }

    fn write_keys(items: &[u8], into: &mut KeyWriter) {
        into.bytes(items);
    }
}

/// As a `u64`, so a key is the same on a 32-bit and a 64-bit target.
impl EvalKey for usize {
    fn write_key(&self, into: &mut KeyWriter) {
        into.bytes(&(*self as u64).to_le_bytes());
    }
}

/// As an `i64`, as `usize` is a `u64`.
impl EvalKey for isize {
    fn write_key(&self, into: &mut KeyWriter) {
        into.bytes(&(*self as i64).to_le_bytes());
    }
}

impl EvalKey for bool {
    fn write_key(&self, into: &mut KeyWriter) {
        into.bytes(&[u8::from(*self)]);
    }
}

impl EvalKey for char {
    fn write_key(&self, into: &mut KeyWriter) {
        into.bytes(&u32::from(*self).to_le_bytes());
    }
}

impl EvalKey for () {
    fn write_key(&self, _: &mut KeyWriter) {}
}

impl EvalKey for str {
    fn write_key(&self, into: &mut KeyWriter) {
        into.count(self.len());
        into.bytes(self.as_bytes());
    }
}

/// A slice: its length, then its elements. A byte slice is this with `u8`, written as one run.
impl<T: EvalKey> EvalKey for [T] {
    fn write_key(&self, into: &mut KeyWriter) {
        into.count(self.len());
        T::write_keys(self, into);
    }
}

/// An array: its elements; its length is its type's.
impl<T: EvalKey, const N: usize> EvalKey for [T; N] {
    fn write_key(&self, into: &mut KeyWriter) {
        T::write_keys(self, into);
    }
}

/// A reference is what it refers to: never its address.
impl<T: EvalKey + ?Sized> EvalKey for &T {
    fn write_key(&self, into: &mut KeyWriter) {
        (**self).write_key(into);
    }
}

impl<T: EvalKey> EvalKey for Option<T> {
    fn write_key(&self, into: &mut KeyWriter) {
        match self {
            None => into.bytes(&[0]),
            Some(value) => {
                into.bytes(&[1]);
                value.write_key(into);
            }
        }
    }
}

impl EvalKey for ContentId {
    fn write_key(&self, into: &mut KeyWriter) {
        into.bytes(&self.0);
    }
}

impl EvalKey for Key {
    fn write_key(&self, into: &mut KeyWriter) {
        self.0.write_key(into);
    }
}

macro_rules! tuple_keys {
    ($(($($head:ident $at:tt,)* ; $last:ident $last_at:tt))+) => {$(
        impl<$($head: EvalKey,)* $last: EvalKey> EvalKey for ($($head,)* $last,) {
            fn write_key(&self, into: &mut KeyWriter) {
                $(self.$at.write_key(into);)*
                into.last(&self.$last_at);
            }
        }
    )+};
}

tuple_keys! {
    (; A 0)
    (A 0, ; B 1)
    (A 0, B 1, ; C 2)
    (A 0, B 1, C 2, ; D 3)
    (A 0, B 1, C 2, D 3, ; E 4)
    (A 0, B 1, C 2, D 3, E 4, ; F 5)
    (A 0, B 1, C 2, D 3, E 4, F 5, ; G 6)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, ; H 7)
}

/// One call of a memo point: the point and the key of its arguments. What a [`Graph`] holds.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Called {
    pub point: Point,
    pub key: Key,
}

/// A call as a [`Handler`] sees it.
#[derive(Clone, Copy, Debug)]
pub struct At {
    pub point: Point,
    pub key: Key,
    /// The call whose body made this one; `None` for a call the program made itself.
    pub parent: Option<Called>,
}

impl At {
    /// The call itself.
    pub fn called(&self) -> Called {
        Called { point: self.point, key: self.key }
    }
}

/// A value given for a call is not of the call's result type: two memo points under one
/// [`Point`], or a handler that kept a value under the wrong call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mistyped {
    pub point: Point,
    /// The result type of the point, by name.
    pub wanted: &'static str,
}

impl fmt::Display for Mistyped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the value given for memo point {} is not a `{}`, its result type: one key covers \
             one value (rule keys-cover-every-input)",
            self.point, self.wanted
        )
    }
}

impl core::error::Error for Mistyped {}

/// A user's evaluator: it is handed every call of the [`Eval`] made from it, nested ones too,
/// and decides where each value comes from. It must leave `next` with a value: by running the
/// body ([`Next::compute`]), by handing the call to another evaluator ([`Next::through`]: so
/// behaviours stack, provenance over a memo, counting over a direct one), or by giving a value
/// it kept ([`Next::supply`]). Returning without one fails the call, naming the handler and the
/// point. As the function is pure, nothing a handler does changes an answer.
pub trait Handler {
    fn run(&self, cx: &Cx, at: At, next: &mut Next<'_>);

    /// What an error calls it.
    fn name(&self) -> &'static str {
        "a handler"
    }
}

/// The rest of a call, as its [`Handler`] holds it: the way to compute its value, and the
/// value once there is one.
pub struct Next<'c> {
    call: &'c mut dyn Call,
}

impl Next<'_> {
    /// Runs the point's body, unless the call has its value already.
    pub fn compute(&mut self) {
        self.call.compute();
    }

    /// Hands the call to `eval`: its provider computes, looks up or passes it on.
    pub fn through(&mut self, cx: &Cx, eval: &Eval<'_>) {
        eval.dispatch(cx, self.call);
    }

    /// Gives the call `value`, a clone of which becomes its result: [`Mistyped`] when it is
    /// not of the point's result type, and the call is as it was.
    pub fn supply(&mut self, value: &dyn Any) -> Result<(), Mistyped> {
        self.call.supply(value)
    }

    /// The call's value, once it has one: for a handler that keeps values of its own.
    pub fn value(&self) -> Option<&dyn Any> {
        self.call.value()
    }

    /// Whether the point's body ran for this call (and not: its value was looked up or given).
    pub fn ran(&self) -> bool {
        self.call.ran()
    }
}

/// How much [`Eval::memo`] keeps.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MemoLimit {
    /// The most entries kept at once, in two generations of half of it each: below 2, nothing
    /// is kept.
    #[blaze::policy]
    pub entries: usize,
    /// The most bytes its two arenas hand out, half each: the entries, their keys and the
    /// maps. An entry that does not fit half of it is not kept.
    #[blaze::policy]
    pub bytes: usize,
}

/// What an [`Eval::memo`] holds and did, from [`Eval::memo_stats`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct MemoStats {
    /// Entries kept now: never more than [`MemoLimit::entries`].
    pub entries: usize,
    /// Bytes its arenas have handed out: never more than [`MemoLimit::bytes`].
    pub bytes: usize,
    /// Calls answered from an entry.
    pub hits: u64,
    /// Calls whose body ran.
    pub computed: u64,
    /// Entries dropped to stay within the limit.
    pub evicted: u64,
    /// Values computed and not kept: no room for one within the limit.
    pub uncached: u64,
}

/// The evaluator: who decides how a memo point's value is got. One concrete type, its provider
/// inside, chosen where it is made; code takes `&Eval`.
pub struct Eval<'a> {
    provider: Provider<'a>,
}

enum Provider<'a> {
    Direct,
    Memo(Memo),
    Handler(&'a dyn Handler),
}

impl<'a> Eval<'a> {
    /// Calls the body: with it a marked call costs a plain call. No key is made.
    pub fn direct() -> Eval<'static> {
        Eval { provider: Provider::Direct }
    }

    /// Remembers results by point and key, within `limit`, in two arenas of `mem` named
    /// `eval.memo`. [`Stopped::OutOfMemory`] when the arenas cannot be made.
    pub fn memo(mem: &Mem, limit: MemoLimit) -> Result<Eval<'static>, Stopped> {
        let young = Generation::new(mem.arena(MEMO_ARENA, limit.bytes / 2)?);
        let old = Generation::new(mem.arena(MEMO_ARENA, limit.bytes / 2)?);
        let memo = Memo {
            limit,
            kept: LocalLock::new(Generations { young, old }),
            hits: Cell::new(0),
            computed: Cell::new(0),
            evicted: Cell::new(0),
            uncached: Cell::new(0),
        };
        Ok(Eval { provider: Provider::Memo(memo) })
    }

    /// Hands every call to `handler`.
    pub fn handler(handler: &'a dyn Handler) -> Eval<'a> {
        Eval { provider: Provider::Handler(handler) }
    }

    /// The value of the memo point `f` of `args`: `f`'s own, whichever provider gives it.
    ///
    /// `F` is a function marked `#[blaze::memo]` ([`MemoPoint`]) of the shape
    /// `fn(&mut Nested<'_>, A) -> R`. `A` is its arguments, a tuple for more than one, each an
    /// [`EvalKey`]. `R: Clone + 'static`: see the module's doc for why by value.
    pub fn call<F, A, R>(&self, cx: &Cx, f: F, args: A) -> R
    where
        F: MemoPoint + Fn(&mut Nested<'_>, A) -> R,
        A: EvalKey,
        R: Clone + 'static,
    {
        self.call_under(cx, None, f, args)
    }

    /// What a remembering evaluator holds and did; `None` of any other.
    pub fn memo_stats(&self) -> Option<MemoStats> {
        match &self.provider {
            Provider::Memo(memo) => Some(memo.stats()),
            Provider::Direct | Provider::Handler(_) => None,
        }
    }

    /// [`Eval::call`] made by the body of `parent`.
    fn call_under<F, A, R>(&self, cx: &Cx, parent: Option<Called>, f: F, args: A) -> R
    where
        F: MemoPoint + Fn(&mut Nested<'_>, A) -> R,
        A: EvalKey,
        R: Clone + 'static,
    {
        if let Provider::Direct = self.provider {
            // A plain call: no point, no key, no erased call.
            let mut running = Running { eval: self, cx, parent: None };
            return f(&mut Nested { door: &mut running }, args);
        }
        let point = Point::of_type::<F>();
        let mut typed = Typed {
            top: self,
            cx,
            point,
            parent,
            f,
            args: Some(args),
            key: None,
            out: None,
            ran: false,
        };
        self.dispatch(cx, &mut typed);
        match typed.out {
            Some(value) => value,
            None => panic!(
                "bstd::eval: {} returned with no value for memo point {point}: a handler's \
                 `run` ends with a value in `next` (`compute`, `through` or `supply`)",
                self.says()
            ),
        }
    }

    /// Gives `call` its value, as this evaluator's provider does.
    fn dispatch(&self, cx: &Cx, call: &mut dyn Call) {
        match &self.provider {
            Provider::Direct => call.compute(),
            Provider::Memo(memo) => memo.run(cx, call),
            Provider::Handler(handler) => {
                let at = At { point: call.point(), key: call.key(), parent: call.parent() };
                handler.run(cx, at, &mut Next { call });
            }
        }
    }

    /// What an error calls its provider.
    fn says(&self) -> &'static str {
        match &self.provider {
            Provider::Direct => "the direct evaluator",
            Provider::Memo(_) => "the remembering evaluator",
            Provider::Handler(handler) => handler.name(),
        }
    }
}

impl fmt::Debug for Eval<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.provider {
            Provider::Direct => write!(f, "Eval::direct"),
            Provider::Memo(memo) => write!(f, "Eval::memo({:?}, {:?})", memo.limit, memo.stats()),
            Provider::Handler(handler) => write!(f, "Eval::handler({})", handler.name()),
        }
    }
}

/// The evaluator as a running memo point sees it: what its body calls other points through.
/// It carries the evaluator the program called, the task's context and the call that is
/// running, so each nested call knows its parent with no global "current point".
///
/// A point takes it by `&mut`: see the module's doc, "Why `&mut Nested`".
pub struct Nested<'a> {
    door: &'a mut dyn Door,
}

impl Nested<'_> {
    /// The value of the memo point `f` of `args`, as [`Eval::call`] gives it, recorded as an
    /// input of the point that is running.
    #[blaze::pure(
        trusted = "the value is `f`'s of `args` whichever evaluator computes, remembers or records it: `f` is a memo point, held pure where it is written, and a handler cannot supply a value of another call unseen (`Mistyped`)"
    )]
    pub fn call<F, A, R>(&mut self, f: F, args: A) -> R
    where
        F: MemoPoint + Fn(&mut Nested<'_>, A) -> R,
        A: EvalKey,
        R: Clone + 'static,
    {
        let at = self.door.running();
        at.eval.call_under(at.cx, at.parent, f, args)
    }

    /// The task's frame, for the point's scratch (`Vec::new_in(points.frame())`): freed when
    /// the task ends. A result must not borrow from it (`R: 'static`).
    #[blaze::pure(
        trusted = "hands out the allocator the task was given; what is allocated in it is the caller's"
    )]
    pub fn frame(&self) -> &Frame {
        self.door.frame()
    }
}

/// Where a running point reaches its evaluator: the one trait a memo point's parameters hold
/// behind `dyn`, trusted for purity as the kernel's `ctx` door is.
#[blaze::pure(
    trusted = "a nested point's value is its function's of its arguments, whichever evaluator computes, remembers or records it"
)]
trait Door {
    fn running(&self) -> Running<'_>;
    fn frame(&self) -> &Frame;
}

/// A point that is running: the evaluator its nested calls go to, and the call they are
/// inputs of (`None` under [`Eval::direct`], which makes no keys).
#[derive(Clone, Copy)]
struct Running<'a> {
    eval: &'a Eval<'a>,
    cx: &'a Cx,
    parent: Option<Called>,
}

impl Door for Running<'_> {
    fn running(&self) -> Running<'_> {
        *self
    }

    fn frame(&self) -> &Frame {
        self.cx.frame()
    }
}

/// One call with its types erased: what a provider and a handler are handed. Its one
/// implementation, [`Typed`], keeps the value typed.
trait Call {
    fn point(&self) -> Point;
    /// The key of the arguments, made once.
    fn key(&mut self) -> Key;
    fn parent(&self) -> Option<Called>;
    /// Runs the body, unless the call has its value already.
    fn compute(&mut self);
    fn ran(&self) -> bool;
    fn value(&self) -> Option<&dyn Any>;
    /// A clone of `kept` as the call's value.
    fn supply(&mut self, kept: &dyn Any) -> Result<(), Mistyped>;
    /// A clone of the call's value in `alloc`; `None` while it has none.
    fn keep(&self, alloc: &Alloc) -> Option<Result<Kept, AllocError>>;
}

struct Typed<'c, 'e, F, A, R> {
    /// The evaluator the program called: where the body's nested calls go, whichever
    /// evaluator a handler passed this call on to.
    top: &'c Eval<'e>,
    cx: &'c Cx,
    point: Point,
    parent: Option<Called>,
    f: F,
    /// The arguments, until the body takes them.
    args: Option<A>,
    key: Option<Key>,
    out: Option<R>,
    ran: bool,
}

impl<F, A, R> Call for Typed<'_, '_, F, A, R>
where
    F: Fn(&mut Nested<'_>, A) -> R,
    A: EvalKey,
    R: Clone + 'static,
{
    fn point(&self) -> Point {
        self.point
    }

    fn key(&mut self) -> Key {
        if let Some(key) = self.key {
            return key;
        }
        let args = self.args.as_ref().expect(
            "bstd::eval: a call's key is made before its body takes the arguments: every \
             provider that keys asks for it first",
        );
        let key = Key::written(args, self.cx.plant() == Plant::KeyIgnoresLast);
        self.key = Some(key);
        key
    }

    fn parent(&self) -> Option<Called> {
        self.parent
    }

    fn compute(&mut self) {
        if self.out.is_some() {
            return;
        }
        // The arguments leave only as the value arrives, so they are here.
        let Some(args) = self.args.take() else { return };
        let this = self.key.map(|key| Called { point: self.point, key });
        let parent = match self.cx.plant() {
            Plant::NestedUnderGrandparent => self.parent,
            _ => this,
        };
        let mut running = Running { eval: self.top, cx: self.cx, parent };
        self.out = Some((self.f)(&mut Nested { door: &mut running }, args));
        self.ran = true;
    }

    fn ran(&self) -> bool {
        self.ran
    }

    fn value(&self) -> Option<&dyn Any> {
        let value: &dyn Any = self.out.as_ref()?;
        Some(value)
    }

    fn supply(&mut self, kept: &dyn Any) -> Result<(), Mistyped> {
        match kept.downcast_ref::<R>() {
            Some(value) => {
                // clone: the one copy a hit costs, in `call`'s `R: Clone`.
                self.out = Some(value.clone());
                Ok(())
            }
            None => Err(Mistyped { point: self.point, wanted: core::any::type_name::<R>() }),
        }
    }

    fn keep(&self, alloc: &Alloc) -> Option<Result<Kept, AllocError>> {
        let value = self.out.as_ref()?;
        // clone: the one copy a miss costs, in `call`'s `R: Clone`; kept in the arena.
        Some(match Box::try_new_in(value.clone(), LocalShared::clone(alloc)) {
            Ok(kept) => {
                let kept: Kept = kept;
                Ok(kept)
            }
            Err(error) => Err(error),
        })
    }
}

/// The name of the arenas of [`Eval::memo`].
const MEMO_ARENA: &str = "eval.memo";

/// A generation's allocator: the owner of its arena's frame, shared by its map and entries.
type Alloc = LocalShared<Frame>;

/// A kept value, in its generation's arena.
type Kept = Box<dyn Any, Alloc>;

/// The state of [`Eval::memo`].
struct Memo {
    limit: MemoLimit,
    kept: LocalLock<Generations>,
    hits: Cell<u64>,
    computed: Cell<u64>,
    evicted: Cell<u64>,
    uncached: Cell<u64>,
}

struct Generations {
    /// Where a new entry goes.
    young: Generation,
    /// What the young generation was before the last rotation: dropped whole at the next.
    old: Generation,
}

struct Generation {
    /// `None` only inside [`Generation::empty`], between dropping the map and making the next.
    entries: Option<Map<Called, Kept, Alloc>>,
    alloc: Alloc,
}

impl Generation {
    fn new(arena: Arena) -> Generation {
        // alloc: once per generation of an evaluator, the owner of its arena's frame.
        let alloc = LocalShared::new(arena.into_frame());
        Generation { entries: Some(Map::new_in(LocalShared::clone(&alloc))), alloc }
    }

    fn get(&self, slot: &Called) -> Option<&Kept> {
        self.entries.as_ref()?.get(slot)
    }

    fn len(&self) -> usize {
        self.entries.as_ref().map_or(0, |entries| entries.len())
    }

    fn used(&self) -> usize {
        self.alloc.used()
    }

    /// Drops every entry and gives the arena's bytes back; how many entries there were.
    fn empty(&mut self) -> usize {
        // The map and its entries go first: each holds the frame.
        let dropped = self.entries.take().map_or(0, |entries| entries.len());
        LocalShared::get_mut(&mut self.alloc)
            .expect(
                "bstd::eval: a generation's frame is held by its map and its entries alone, \
                 which were dropped",
            )
            .reset();
        self.entries = Some(Map::new_in(LocalShared::clone(&self.alloc)));
        dropped
    }

    /// Keeps a clone of `call`'s value under `slot`, if the arena has room for it.
    fn keep(&mut self, slot: Called, call: &dyn Call) -> bool {
        let Some(entries) = self.entries.as_mut() else { return false };
        if entries.try_reserve(1).is_err() {
            return false;
        }
        match call.keep(&self.alloc) {
            Some(Ok(kept)) => {
                entries.insert(slot, kept);
                true
            }
            Some(Err(AllocError)) | None => false,
        }
    }
}

impl Memo {
    fn stats(&self) -> MemoStats {
        let kept = self.kept.borrow();
        MemoStats {
            entries: kept.young.len() + kept.old.len(),
            bytes: kept.young.used() + kept.old.used(),
            hits: self.hits.get(),
            computed: self.computed.get(),
            evicted: self.evicted.get(),
            uncached: self.uncached.get(),
        }
    }

    /// Gives `call` its value: its entry's, or the body's, which is then kept.
    fn run(&self, cx: &Cx, call: &mut dyn Call) {
        let plant = cx.plant();
        let point = match plant {
            Plant::MemoSharesPoints => Point::none(),
            _ => call.point(),
        };
        let slot = Called { point, key: call.key() };
        {
            let kept = self.kept.borrow();
            if let Some(entry) = kept.young.get(&slot).or_else(|| kept.old.get(&slot)) {
                // The value, not its box: `**entry` is the `dyn Any` the box holds.
                match call.supply(&**entry) {
                    Ok(()) => {
                        self.hits.set(self.hits.get() + 1);
                        return;
                    }
                    // Two values of two types under one point and key: refused, never
                    // recomputed over (rule keys-cover-every-input).
                    Err(mistyped) => panic!("bstd::eval: {mistyped}"),
                }
            }
        }
        // No borrow is held here: the body's nested calls come back through this evaluator.
        call.compute();
        self.computed.set(self.computed.get() + 1);
        self.keep(plant, slot, call);
    }

    /// Keeps `call`'s value under `slot`, dropping the old generation first when the young
    /// one is at its half of a limit.
    fn keep(&self, plant: Plant, slot: Called, call: &dyn Call) {
        let half = self.limit.entries / 2;
        let evicts = plant != Plant::MemoNeverEvicts;
        let mut kept = self.kept.borrow_mut();
        if half == 0 {
            self.uncached.set(self.uncached.get() + 1);
            return;
        }
        if kept.young.len() >= half && evicts {
            self.rotate(&mut kept);
        }
        if kept.young.keep(slot, call) {
            return;
        }
        // The young arena is full. Emptied, it has its whole half: a value that does not fit
        // then is over the limit by itself.
        if evicts && kept.young.len() > 0 {
            self.rotate(&mut kept);
            if kept.young.keep(slot, call) {
                return;
            }
        }
        self.uncached.set(self.uncached.get() + 1);
    }

    /// Drops the old generation whole; the young one becomes the old, and the emptied one
    /// the young.
    fn rotate(&self, kept: &mut Generations) {
        let dropped = kept.old.empty();
        self.evicted.set(self.evicted.get() + dropped as u64);
        core::mem::swap(&mut kept.young, &mut kept.old);
    }
}

/// What was computed from what: for each call of a memo point, the calls its body made, in
/// the order it first made them. Its caller owns it, makes it in an arena of its own, and
/// reads it once the work is done; [`Provenance`] writes it.
pub struct Graph<'m> {
    arena: &'m Arena,
    edges: LocalLock<Map<Called, Vec<Called, &'m Frame>, &'m Frame>>,
    /// The calls the program made itself, in the order it first made them.
    roots: LocalLock<Vec<Called, &'m Frame>>,
    /// The first call that could not be recorded: the arena had no room.
    failed: LocalLock<Option<Stopped>>,
}

impl<'m> Graph<'m> {
    /// An empty graph in `arena`.
    pub fn new_in(arena: &'m Arena) -> Graph<'m> {
        Graph {
            arena,
            edges: LocalLock::new(Map::new_in(arena.alloc())),
            roots: LocalLock::new(Vec::new_in(arena.alloc())),
            failed: LocalLock::new(None),
        }
    }

    /// The calls the body of `point` with the arguments of `key` made, read by `read`, in the
    /// order it first made them: `None` when no such call was recorded. An error when the
    /// arena ran out while recording: the graph is then not whole, and says so rather than
    /// answer with what it has.
    pub fn inputs_of<T>(
        &self,
        point: Point,
        key: &Key,
        read: impl FnOnce(&[Called]) -> T,
    ) -> Result<Option<T>, Stopped> {
        self.whole()?;
        let edges = self.edges.borrow();
        Ok(edges.get(&Called { point, key: *key }).map(|inputs| read(inputs.as_slice())))
    }

    /// The calls the program made itself, read by `read`, in the order it first made them.
    pub fn roots<T>(&self, read: impl FnOnce(&[Called]) -> T) -> Result<T, Stopped> {
        self.whole()?;
        let roots = self.roots.borrow();
        Ok(read(roots.as_slice()))
    }

    /// How many distinct calls were recorded.
    pub fn len(&self) -> usize {
        self.edges.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether every call was recorded.
    fn whole(&self) -> Result<(), Stopped> {
        match &*self.failed.borrow() {
            // clone: an error's few words, once per read of a graph that is not whole
            Some(stopped) => Err(stopped.clone()),
            None => Ok(()),
        }
    }

    /// Records the call `made` by the body of `parent`, or by the program.
    fn record(&self, parent: Option<Called>, made: Called) {
        if self.failed.borrow().is_some() {
            return;
        }
        if let Err(stopped) = self.try_record(parent, made) {
            *self.failed.borrow_mut() = Some(stopped);
        }
    }

    fn try_record(&self, parent: Option<Called>, made: Called) -> Result<(), Stopped> {
        let frame = self.arena.alloc();
        let full = |asked: usize| Stopped::OutOfMemory { arena: self.arena.name(), asked };
        let mut edges = self.edges.borrow_mut();
        // Every call is a node, with inputs or without.
        for node in [Some(made), parent].into_iter().flatten() {
            if !edges.contains_key(&node) {
                edges.try_reserve(1).map_err(|_| full(size_of::<Called>()))?;
                edges.insert(node, Vec::new_in(frame));
            }
        }
        match parent {
            Some(parent) => {
                let Some(inputs) = edges.get_mut(&parent) else { return Ok(()) };
                if !inputs.contains(&made) {
                    inputs.try_reserve(1).map_err(|_| full(size_of::<Called>()))?;
                    inputs.push(made);
                }
            }
            None => {
                let mut roots = self.roots.borrow_mut();
                if !roots.contains(&made) {
                    roots.try_reserve(1).map_err(|_| full(size_of::<Called>()))?;
                    roots.push(made);
                }
            }
        }
        Ok(())
    }
}

impl fmt::Debug for Graph<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Graph({} calls in {})", self.len(), self.arena.name())
    }
}

/// A [`Handler`] that records what was computed from what: each call it is handed goes into
/// its [`Graph`] as an input of the call whose body made it, and then on to the evaluator it
/// wraps. Made the outermost evaluator, it sees every call, a remembered one too:
///
/// ```ignore
/// let graph = Graph::new_in(&arena);
/// let inner = Eval::memo(&mem, limit)?;
/// let provenance = Provenance::new(&inner, &graph);
/// let eval = Eval::handler(&provenance);
/// ..
/// graph.inputs_of(Point::of(&size_of), &Key::of(&(&s, &target)), |inputs| ..)?;
/// ```
///
/// A call answered from the wrapped evaluator's memory runs no body, so it adds no inputs:
/// they were recorded when its body ran, and are the same, as the point is pure.
pub struct Provenance<'a, 'm> {
    inner: &'a Eval<'a>,
    graph: &'a Graph<'m>,
}

impl<'a, 'm> Provenance<'a, 'm> {
    pub fn new(inner: &'a Eval<'a>, graph: &'a Graph<'m>) -> Provenance<'a, 'm> {
        Provenance { inner, graph }
    }
}

impl Handler for Provenance<'_, '_> {
    fn run(&self, cx: &Cx, at: At, next: &mut Next<'_>) {
        self.graph.record(at.parent, at.called());
        next.through(cx, self.inner);
    }

    fn name(&self) -> &'static str {
        "the provenance handler"
    }
}
